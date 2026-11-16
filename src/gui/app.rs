//! GUI state and the messages that change it. Drawing (in `view`) reads
//! this state, edits text buffers in place, and turns clicks into `Msg`s
//! that only [`App::update`] applies. Slow work runs on threads that
//! report back through a channel.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::Result;
use eframe::egui;

use orbis::config::Config;
use orbis::download::DownloadEvent;
use orbis::game::Dlc;
use orbis::instance::{self, CopyMode, CreateOptions, Instance, ModSource, PrefixMode, Registry};
use orbis::launch;
use orbis::manual::{self, AddOptions, ManualMod};
use orbis::metadata::Module;
use orbis::ops;
use orbis::paths::Paths;
use orbis::repo::{self, Index};
use orbis::resolver::{Plan, Request, ResolveError};
use orbis::steam::SteamInfo;
use orbis::version::{GameVersion, ModVersion};

use super::Options;
use crate::cli::{self, Context};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Mods,
    Manual,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Installed,
    Available,
    Upgradable,
    All,
}

impl Filter {
    pub const ALL: [Filter; 4] = [
        Filter::Installed,
        Filter::Available,
        Filter::Upgradable,
        Filter::All,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Filter::Installed => "Installed",
            Filter::Available => "Compatible",
            Filter::Upgradable => "Upgradable",
            Filter::All => "All",
        }
    }
}

/// One instance and what's in it.
pub struct InstanceView {
    pub inst: Instance,
    pub reg: Registry,
    pub dlc: Vec<Dlc>,
}

/// One line of the mod table.
#[derive(Clone, Debug)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub installed: Option<ModVersion>,
    /// Newest release compatible with the instance.
    pub latest: Option<ModVersion>,
    pub game: String,
    pub downloads: u64,
    pub manual: bool,
    pub auto: bool,
    pub dlc: bool,
}

impl Row {
    pub fn upgradable(&self) -> bool {
        !self.manual && matches!((&self.installed, &self.latest), (Some(i), Some(l)) if l > i)
    }
}

/// Changes the user has queued but not applied.
#[derive(Clone, Debug, Default)]
pub struct Queue {
    pub install: BTreeSet<String>,
    pub remove: BTreeSet<String>,
    pub upgrade: BTreeSet<String>,
}

impl Queue {
    pub fn is_empty(&self) -> bool {
        self.install.is_empty() && self.remove.is_empty() && self.upgrade.is_empty()
    }

    pub fn len(&self) -> usize {
        self.install.len() + self.remove.len() + self.upgrade.len()
    }
}

/// Text buffers for the instance settings form.
#[derive(Clone, Debug, Default)]
pub struct SettingsForm {
    pub for_instance: Option<String>,
    pub name: String,
    pub compat: String,
    pub allow_testing: bool,
    pub prefix_isolated: bool,
    pub args: String,
    pub env: String,
    pub notes: String,
}

/// Text buffers for the global settings form.
#[derive(Clone, Debug, Default)]
pub struct GlobalForm {
    pub loaded: bool,
    pub steam_root: String,
    pub proton: String,
    pub use_runtime: bool,
    pub workers: String,
    pub repos: String,
}

/// Fields of the "add manual mod" dialog.
#[derive(Clone, Debug, Default)]
pub struct AddManualForm {
    pub path: String,
    pub name: String,
    pub version: String,
    pub provides: String,
    pub depends: String,
    pub conflicts: String,
    pub find: String,
    pub url: String,
    pub notes: String,
    /// Destination folders the archive would install into, or an error.
    pub preview: Option<Result<Vec<String>, String>>,
    pub previewed_for: String,
}

/// A modal dialog.
pub enum Dialog {
    NewInstance {
        name: String,
        source: String,
        with_saves: bool,
        keep_mods: bool,
    },
    CloneInstance {
        from: String,
        name: String,
        with_saves: bool,
    },
    DeleteInstance {
        id: String,
        name: String,
    },
    Confirm {
        plan: Plan,
        picks: Vec<bool>,
        manual: Option<ManualMod>,
    },
    Choice {
        by: String,
        relationship: String,
        candidates: Vec<String>,
        pick: usize,
        manual: Option<ManualMod>,
    },
    AddManual(Box<AddManualForm>),
    DeleteManual(ManualMod),
    Error(String),
}

/// What the view asks for.
pub enum Msg {
    SelectInstance(String),
    SetTab(Tab),
    SetFilter(Filter),
    SelectMod(String),
    ToggleInstall(String),
    ToggleRemove(String),
    ToggleUpgrade(String),
    QueueAllUpgrades,
    ClearQueue,
    ApplyQueue,
    ConfirmApply,
    ChoicePicked,
    UpdateIndex,
    OpenNewInstance,
    CreateInstance,
    OpenClone,
    CloneInstance,
    OpenDelete,
    DeleteInstance,
    Launch,
    OpenFolder(PathBuf),
    OpenUrl(String),
    OpenAddManual(Option<PathBuf>),
    PreviewManual,
    AddManual,
    InstallManual(String),
    RemoveManual(String),
    AskDeleteManual(String),
    DeleteManual,
    SaveInstanceSettings,
    SaveGlobalSettings,
    CloseDialog,
    Refresh,
}

/// Results of background work.
pub enum Done {
    IndexLoaded(Result<Option<Index>, String>),
    IndexUpdated(Result<(repo::UpdateSummary, Index), String>),
    Applied(Result<(), String>),
    InstanceCreated(Result<Instance, String>),
    ManualAdded(Result<ManualMod, String>),
}

/// Messages from background threads.
pub enum TaskMsg {
    Status(String),
    Download {
        id: String,
        bytes: u64,
        total: Option<u64>,
    },
    DownloadDone(String),
    Copy(usize, usize),
    Done(Box<Done>),
}

/// A running background job.
pub struct Task {
    pub title: String,
    pub status: String,
    pub progress: Option<f32>,
    pub downloads: BTreeMap<String, (u64, Option<u64>)>,
    rx: Receiver<TaskMsg>,
}

/// All GUI state.
pub struct App {
    pub ctx: Context,
    pub egui: egui::Context,
    pub steam: SteamInfo,
    pub instances: Vec<InstanceView>,
    pub selected: Option<String>,
    pub index: Option<Arc<Index>>,
    pub index_status: String,
    pub tab: Tab,
    pub filter: Filter,
    pub search: String,
    pub rows: Vec<Row>,
    rows_key: Option<(String, Filter, String, u64, usize)>,
    pub selected_mod: Option<String>,
    pub queue: Queue,
    pub choices: HashMap<String, String>,
    pub dialog: Option<Dialog>,
    pub task: Option<Task>,
    pub messages: Vec<String>,
    pub manual: Vec<ManualMod>,
    pub settings: SettingsForm,
    pub global: GlobalForm,
    pub running: Vec<(String, Child)>,
    generation: u64,
}

impl App {
    pub fn new(ctx: Context, opts: &Options, egui: egui::Context) -> Self {
        let steam = ctx.steam();
        let mut app = Self {
            ctx,
            egui,
            steam,
            instances: Vec::new(),
            selected: None,
            index: None,
            index_status: "Loading mod index…".into(),
            tab: Tab::Mods,
            filter: Filter::Installed,
            search: opts.search.clone().unwrap_or_default(),
            rows: Vec::new(),
            rows_key: None,
            selected_mod: opts.select.clone(),
            queue: Queue::default(),
            choices: HashMap::new(),
            dialog: None,
            task: None,
            messages: Vec::new(),
            manual: Vec::new(),
            settings: SettingsForm::default(),
            global: GlobalForm::default(),
            running: Vec::new(),
            generation: 0,
        };
        app.reload();
        if let Some(key) = &opts.instance
            && let Some(v) = app
                .instances
                .iter()
                .find(|v| &v.inst.id == key || v.inst.config.name.eq_ignore_ascii_case(key))
        {
            app.selected = Some(v.inst.id.clone());
        }
        app.tab = match opts.tab.as_deref() {
            Some("manual") => Tab::Manual,
            Some("settings") => Tab::Settings,
            _ => Tab::Mods,
        };
        app.filter = match opts.filter.as_deref() {
            Some("available") => Filter::Available,
            Some("upgradable") => Filter::Upgradable,
            Some("all") => Filter::All,
            _ => Filter::Installed,
        };
        app.egui.global_style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(8.0, 4.0);
        });
        app
    }

    /// Re-reads instances and the manual library from disk.
    pub fn reload(&mut self) {
        match instance::list(&self.ctx.paths) {
            Ok(list) => {
                self.instances = list
                    .into_iter()
                    .map(|inst| {
                        let reg = inst.registry().unwrap_or_else(|e| {
                            log::warn!("{}: {e:#}", inst.config.name);
                            Registry::default()
                        });
                        let dlc = inst.dlc();
                        InstanceView { inst, reg, dlc }
                    })
                    .collect();
            }
            Err(e) => self.error(format!("{e:#}")),
        }
        match manual::list(&self.ctx.paths) {
            Ok(m) => self.manual = m,
            Err(e) => self.error(format!("{e:#}")),
        }
        if self
            .selected
            .as_ref()
            .is_none_or(|s| !self.instances.iter().any(|v| &v.inst.id == s))
        {
            self.selected = self.instances.first().map(|v| v.inst.id.clone());
        }
        self.generation += 1;
        self.settings.for_instance = None;
    }

    pub fn current(&self) -> Option<&InstanceView> {
        let id = self.selected.as_ref()?;
        self.instances.iter().find(|v| &v.inst.id == id)
    }

    fn error(&mut self, msg: String) {
        log::error!("{msg}");
        self.messages.push(msg.clone());
        self.dialog = Some(Dialog::Error(msg));
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.messages.push(msg.into());
    }

    pub fn last_message(&self) -> &str {
        self.messages.last().map_or("", String::as_str)
    }

    /// One frame: drain background messages, draw, apply what was clicked.
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        self.poll();
        self.handle_drops(ui.ctx());
        self.ensure_rows();
        let mut msgs = Vec::new();
        super::view::draw(self, ui, &mut msgs);
        for m in msgs {
            self.update(m);
        }
        if self.task.is_some() || !self.running.is_empty() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if let Some(p) = dropped.into_iter().next() {
            self.tab = Tab::Manual;
            self.update(Msg::OpenAddManual(Some(p)));
        }
    }

    // ---- background work --------------------------------------------

    fn spawn(&mut self, title: &str, work: impl FnOnce(&Sender<TaskMsg>) -> Done + Send + 'static) {
        if self.task.is_some() {
            self.note("Another task is still running.");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let egui = self.egui.clone();
        std::thread::spawn(move || {
            let done = work(&tx);
            let _ = tx.send(TaskMsg::Done(Box::new(done)));
            egui.request_repaint();
        });
        self.task = Some(Task {
            title: title.to_owned(),
            status: String::new(),
            progress: None,
            downloads: BTreeMap::new(),
            rx,
        });
    }

    /// Loads the cached index in the background.
    pub fn start_index_load(&mut self) {
        let paths = self.ctx.paths.clone();
        self.spawn("Loading mod index", move |_| {
            Done::IndexLoaded(Index::load(&paths).map_err(|e| format!("{e:#}")))
        });
    }

    /// Loads the index on this thread (headless mode).
    pub fn load_index_now(&mut self) {
        let r = Index::load(&self.ctx.paths).map_err(|e| format!("{e:#}"));
        self.finish(Done::IndexLoaded(r));
    }

    /// Whether a background task is running.
    pub fn busy(&self) -> bool {
        self.task.is_some()
    }

    pub fn poll(&mut self) {
        let mut done = Vec::new();
        if let Some(task) = &mut self.task {
            while let Ok(msg) = task.rx.try_recv() {
                match msg {
                    TaskMsg::Status(s) => task.status = s,
                    TaskMsg::Download { id, bytes, total } => {
                        task.downloads.insert(id, (bytes, total));
                    }
                    TaskMsg::DownloadDone(id) => {
                        task.downloads.remove(&id);
                    }
                    TaskMsg::Copy(d, t) => {
                        task.status = format!("Copying game files: {d}/{t}");
                        task.progress = (t > 0).then(|| d as f32 / t as f32);
                    }
                    TaskMsg::Done(d) => done.push(*d),
                }
            }
        }
        for d in done {
            self.task = None;
            self.finish(d);
        }
        self.running
            .retain_mut(|(name, child)| match child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    log::info!("{name} exited: {status}");
                    false
                }
                Err(_) => false,
            });
    }

    fn finish(&mut self, done: Done) {
        match done {
            Done::IndexLoaded(Ok(Some(index))) => {
                self.index_status = index_label(&index);
                self.index = Some(Arc::new(index));
                self.generation += 1;
            }
            Done::IndexLoaded(Ok(None)) => {
                self.index_status = "No mod index yet: press Update".into();
            }
            Done::IndexLoaded(Err(e)) => {
                self.index_status = "Mod index unreadable: press Update".into();
                self.error(e);
            }
            Done::IndexUpdated(Ok((summary, index))) => {
                self.note(if summary.unchanged {
                    format!("Mod index unchanged ({} mods)", summary.modules)
                } else {
                    format!(
                        "Mod index updated: {} mods, {} releases",
                        summary.modules, summary.releases
                    )
                });
                self.index_status = index_label(&index);
                self.index = Some(Arc::new(index));
                self.generation += 1;
            }
            Done::IndexUpdated(Err(e)) => self.error(format!("Update failed: {e}")),
            Done::Applied(r) => {
                self.reload();
                match r {
                    Ok(()) => {
                        self.queue = Queue::default();
                        self.choices.clear();
                        self.note("Changes applied");
                    }
                    Err(e) => self.error(format!("Applying changes failed: {e}")),
                }
            }
            Done::InstanceCreated(r) => match r {
                Ok(inst) => {
                    self.note(format!("Created instance {}", inst.config.name));
                    self.selected = Some(inst.id.clone());
                    self.reload();
                }
                Err(e) => self.error(format!("Creating the instance failed: {e}")),
            },
            Done::ManualAdded(r) => match r {
                Ok(m) => {
                    self.note(format!("Added {} to the manual library", m.name));
                    self.reload();
                }
                Err(e) => self.error(format!("Adding the mod failed: {e}")),
            },
        }
    }

    // ---- rows ---------------------------------------------------------

    fn ensure_rows(&mut self) {
        let key = (
            self.selected.clone().unwrap_or_default(),
            self.filter,
            self.search.trim().to_lowercase(),
            self.generation,
            self.queue.len(),
        );
        if self.rows_key.as_ref() == Some(&key) {
            return;
        }
        self.rows = self.build_rows(&key.2);
        self.rows_key = Some(key);
    }

    fn build_rows(&self, query: &str) -> Vec<Row> {
        let Some(view) = self.current() else {
            return vec![];
        };
        let compat = view.inst.compat_versions();
        let testing = view.inst.config.allow_testing;
        let mut rows = Vec::new();
        let empty = Index::default();
        let index: &Index = self.index.as_deref().unwrap_or(&empty);
        for (id, m) in &view.reg.mods {
            let latest = index
                .latest_compatible(id, &compat, testing)
                .map(|l| l.version.clone());
            if !cli::matches(&m.module, query) {
                continue;
            }
            rows.push(Row {
                id: id.clone(),
                name: m.module.display_name().to_owned(),
                summary: m.module.summary.clone(),
                installed: Some(m.module.version.clone()),
                latest,
                game: m.module.game_range_label(),
                downloads: index.download_count(id),
                manual: m.source == ModSource::Manual,
                auto: m.auto_installed,
                dlc: false,
            });
        }
        for d in &view.dlc {
            if query.is_empty() || d.identifier.to_lowercase().contains(query) {
                rows.push(Row {
                    id: d.identifier.clone(),
                    name: d.identifier.clone(),
                    summary: "Expansion (detected)".into(),
                    installed: Some(d.version.clone().unwrap_or_else(|| ModVersion::new("?"))),
                    latest: None,
                    game: String::new(),
                    downloads: 0,
                    manual: false,
                    auto: false,
                    dlc: true,
                });
            }
        }
        match self.filter {
            Filter::Installed => {
                rows.sort_by_key(|r| r.name.to_lowercase());
                return rows;
            }
            Filter::Upgradable => {
                rows.retain(|r| r.upgradable());
                rows.sort_by_key(|r| r.name.to_lowercase());
                return rows;
            }
            Filter::Available | Filter::All => {}
        }
        rows.clear();
        for (id, versions) in index.iter() {
            let installed = view.reg.mods.get(id);
            let latest = index.latest_compatible(id, &compat, testing);
            if self.filter == Filter::Available && (latest.is_none() || installed.is_some()) {
                continue;
            }
            let Some(shown) = latest.or(versions.first()) else {
                continue;
            };
            if shown.kind == orbis::metadata::Kind::Dlc || !cli::matches(shown, query) {
                continue;
            }
            rows.push(Row {
                id: id.clone(),
                name: shown.display_name().to_owned(),
                summary: shown.summary.clone(),
                installed: installed.map(|m| m.module.version.clone()),
                latest: latest.map(|l| l.version.clone()),
                game: shown.game_range_label(),
                downloads: index.download_count(id),
                manual: false,
                auto: installed.is_some_and(|m| m.auto_installed),
                dlc: false,
            });
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.downloads));
        rows
    }

    /// The module to show in the details pane.
    pub fn selected_module(&self) -> Option<Module> {
        let id = self.selected_mod.as_ref()?;
        if let Some(view) = self.current()
            && let Some(m) = view.reg.mods.get(id)
        {
            let index_latest = self
                .index
                .as_ref()
                .and_then(|i| i.versions(id).first().cloned());
            return Some(index_latest.unwrap_or_else(|| m.module.clone()));
        }
        let index = self.index.as_ref()?;
        if let Some(view) = self.current() {
            let compat = view.inst.compat_versions();
            if let Some(m) = index.latest_compatible(id, &compat, view.inst.config.allow_testing) {
                return Some(m.clone());
            }
        }
        index.versions(id).first().cloned()
    }

    // ---- update ---------------------------------------------------------

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::SelectInstance(id) => {
                if self.selected.as_ref() != Some(&id) {
                    self.selected = Some(id);
                    self.queue = Queue::default();
                    self.choices.clear();
                    self.settings.for_instance = None;
                    self.selected_mod = None;
                }
            }
            Msg::SetTab(t) => self.tab = t,
            Msg::SetFilter(f) => self.filter = f,
            Msg::SelectMod(id) => self.selected_mod = Some(id),
            Msg::ToggleInstall(id) => {
                if !self.queue.install.remove(&id) {
                    self.queue.install.insert(id);
                }
            }
            Msg::ToggleRemove(id) => {
                self.queue.upgrade.remove(&id);
                if !self.queue.remove.remove(&id) {
                    self.queue.remove.insert(id);
                }
            }
            Msg::ToggleUpgrade(id) => {
                self.queue.remove.remove(&id);
                if !self.queue.upgrade.remove(&id) {
                    self.queue.upgrade.insert(id);
                }
            }
            Msg::QueueAllUpgrades => {
                let ids: Vec<String> = self
                    .rows
                    .iter()
                    .filter(|r| r.upgradable())
                    .map(|r| r.id.clone())
                    .collect();
                if let Some(view) = self.current()
                    && let Some(index) = &self.index
                {
                    for (id, _) in ops::upgradable(&view.inst, index, &view.reg) {
                        self.queue.upgrade.insert(id);
                    }
                }
                self.queue.upgrade.extend(ids);
            }
            Msg::ClearQueue => {
                self.queue = Queue::default();
                self.choices.clear();
            }
            Msg::ApplyQueue => self.plan_queue(),
            Msg::ConfirmApply => self.confirm_apply(),
            Msg::ChoicePicked => {
                if let Some(Dialog::Choice {
                    relationship,
                    candidates,
                    pick,
                    manual,
                    ..
                }) = self.dialog.take()
                {
                    if let Some(c) = candidates.get(pick) {
                        self.choices.insert(relationship, c.clone());
                    }
                    match manual {
                        Some(m) => self.plan_manual(m),
                        None => self.plan_queue(),
                    }
                }
            }
            Msg::UpdateIndex => {
                let paths = self.ctx.paths.clone();
                let config = self.ctx.config.clone();
                self.spawn("Updating mod index", move |tx| {
                    let mut report = |s: String| {
                        let _ = tx.send(TaskMsg::Status(s));
                    };
                    let r = repo::update(&paths, &config, &mut report).and_then(|s| {
                        let index = Index::load(&paths)?.unwrap_or_default();
                        Ok((s, index))
                    });
                    Done::IndexUpdated(r.map_err(|e| format!("{e:#}")))
                });
            }
            Msg::OpenNewInstance => {
                let n = self.instances.len() + 1;
                self.dialog = Some(Dialog::NewInstance {
                    name: format!("KSP {n}"),
                    source: self
                        .steam
                        .ksp
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    with_saves: false,
                    keep_mods: false,
                });
            }
            Msg::CreateInstance => {
                if let Some(Dialog::NewInstance {
                    name,
                    source,
                    with_saves,
                    keep_mods,
                }) = self.dialog.take()
                {
                    let paths = self.ctx.paths.clone();
                    let opts = CreateOptions {
                        name,
                        source: PathBuf::from(source.trim()),
                        stock_only: !keep_mods,
                        with_saves,
                        mode: CopyMode::Copy,
                    };
                    self.spawn("Creating instance", move |tx| {
                        let mut p = |d, t| {
                            let _ = tx.send(TaskMsg::Copy(d, t));
                        };
                        Done::InstanceCreated(
                            instance::create(&paths, &opts, &mut p).map_err(|e| format!("{e:#}")),
                        )
                    });
                }
            }
            Msg::OpenClone => {
                if let Some(v) = self.current() {
                    self.dialog = Some(Dialog::CloneInstance {
                        from: v.inst.id.clone(),
                        name: format!("{} copy", v.inst.config.name),
                        with_saves: true,
                    });
                }
            }
            Msg::CloneInstance => {
                if let Some(Dialog::CloneInstance {
                    from,
                    name,
                    with_saves,
                }) = self.dialog.take()
                    && let Some(src) = self
                        .instances
                        .iter()
                        .find(|v| v.inst.id == from)
                        .map(|v| v.inst.clone())
                {
                    let paths = self.ctx.paths.clone();
                    self.spawn("Cloning instance", move |tx| {
                        let mut p = |d, t| {
                            let _ = tx.send(TaskMsg::Copy(d, t));
                        };
                        Done::InstanceCreated(
                            instance::clone_instance(&paths, &src, &name, with_saves, &mut p)
                                .map_err(|e| format!("{e:#}")),
                        )
                    });
                }
            }
            Msg::OpenDelete => {
                if let Some(v) = self.current() {
                    self.dialog = Some(Dialog::DeleteInstance {
                        id: v.inst.id.clone(),
                        name: v.inst.config.name.clone(),
                    });
                }
            }
            Msg::DeleteInstance => {
                if let Some(Dialog::DeleteInstance { id, name }) = self.dialog.take()
                    && let Some(v) = self.instances.iter().find(|v| v.inst.id == id)
                {
                    match instance::delete(&v.inst) {
                        Ok(()) => {
                            self.note(format!("Deleted {name}"));
                            self.selected = None;
                            self.reload();
                        }
                        Err(e) => self.error(format!("{e:#}")),
                    }
                }
            }
            Msg::Launch => self.launch(),
            Msg::OpenFolder(p) => open_external(&p.to_string_lossy()),
            Msg::OpenUrl(u) => open_external(&u),
            Msg::OpenAddManual(path) => {
                let mut form = AddManualForm {
                    version: "1.0".into(),
                    ..AddManualForm::default()
                };
                if let Some(p) = path {
                    form.name = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().replace(['_', '-'], " "))
                        .unwrap_or_default();
                    form.path = p.display().to_string();
                }
                self.dialog = Some(Dialog::AddManual(Box::new(form)));
                self.update(Msg::PreviewManual);
            }
            Msg::PreviewManual => {
                if let Some(Dialog::AddManual(form)) = &mut self.dialog {
                    form.preview = preview_manual(form);
                    form.previewed_for = format!("{}|{}", form.path, form.find);
                }
            }
            Msg::AddManual => {
                if let Some(Dialog::AddManual(form)) = self.dialog.take() {
                    let paths = self.ctx.paths.clone();
                    let opts = add_options(&form);
                    self.spawn("Adding manual mod", move |_| {
                        Done::ManualAdded(manual::add(&paths, &opts).map_err(|e| format!("{e:#}")))
                    });
                }
            }
            Msg::InstallManual(id) => match manual::find(&self.ctx.paths, &id) {
                Ok(m) => self.plan_manual(m),
                Err(e) => self.error(format!("{e:#}")),
            },
            Msg::RemoveManual(id) => {
                self.queue = Queue::default();
                self.queue.remove.insert(format!("{}{id}", manual::PREFIX));
                self.plan_queue();
            }
            Msg::AskDeleteManual(id) => {
                if let Some(m) = self.manual.iter().find(|m| m.id == id) {
                    self.dialog = Some(Dialog::DeleteManual(m.clone()));
                }
            }
            Msg::DeleteManual => {
                if let Some(Dialog::DeleteManual(m)) = self.dialog.take() {
                    match manual::delete(&self.ctx.paths, &m) {
                        Ok(()) => {
                            self.note(format!("Deleted {} from the library", m.name));
                            self.reload();
                        }
                        Err(e) => self.error(format!("{e:#}")),
                    }
                }
            }
            Msg::SaveInstanceSettings => self.save_instance_settings(),
            Msg::SaveGlobalSettings => self.save_global_settings(),
            Msg::CloseDialog => self.dialog = None,
            Msg::Refresh => {
                self.steam = self.ctx.steam();
                self.reload();
            }
        }
    }

    fn plan_queue(&mut self) {
        let Some(view) = self.current() else { return };
        let needs_index = !self.queue.install.is_empty() || !self.queue.upgrade.is_empty();
        let index = match self.index.clone() {
            Some(i) => i,
            None if !needs_index => Arc::new(Index::default()),
            None => {
                self.error("The mod index isn't loaded yet: press Update index.".into());
                return;
            }
        };
        let req = Request {
            install: self
                .queue
                .install
                .iter()
                .map(|id| (id.clone(), None))
                .collect(),
            remove: self.queue.remove.iter().cloned().collect(),
            upgrade: self.queue.upgrade.iter().cloned().collect(),
            choices: self.choices.clone(),
        };
        let result = ops::plan(&view.inst, &index, &req);
        self.show_plan(result, None);
    }

    fn plan_manual(&mut self, m: ManualMod) {
        let Some(view) = self.current() else { return };
        let empty = Arc::new(Index::default());
        let index = self.index.clone().unwrap_or(empty);
        let result = ops::plan_manual(&view.inst, &index, &m);
        self.show_plan(result, Some(m));
    }

    fn show_plan(&mut self, result: Result<Result<Plan, ResolveError>>, manual: Option<ManualMod>) {
        match result {
            Err(e) => self.error(format!("{e:#}")),
            Ok(Err(ResolveError::Choice {
                by,
                relationship,
                candidates,
            })) => {
                self.dialog = Some(Dialog::Choice {
                    by,
                    relationship,
                    candidates,
                    pick: 0,
                    manual,
                });
            }
            Ok(Err(e)) => self.error(e.to_string()),
            Ok(Ok(plan)) if plan.is_empty() => self.note("Nothing to do"),
            Ok(Ok(plan)) => {
                let picks = plan.suggestions.iter().map(|_| false).collect();
                self.dialog = Some(Dialog::Confirm {
                    plan,
                    picks,
                    manual,
                });
            }
        }
    }

    fn confirm_apply(&mut self) {
        let Some(Dialog::Confirm {
            plan,
            picks,
            manual,
        }) = self.dialog.take()
        else {
            return;
        };
        let chosen: Vec<String> = plan
            .suggestions
            .iter()
            .zip(&picks)
            .filter(|(_, p)| **p)
            .map(|(s, _)| {
                self.choices
                    .get(&s.relationship)
                    .cloned()
                    .unwrap_or_else(|| s.candidates[0].clone())
            })
            .collect();
        if !chosen.is_empty() {
            // Re-plan with the picked recommendations included.
            self.queue.install.extend(chosen);
            if manual.is_some() {
                // Install the manual mod now; the picks wait in the queue.
                self.start_apply(plan);
                self.note("Picked recommendations were added to the change queue");
            } else {
                self.plan_queue();
            }
            return;
        }
        self.start_apply(plan);
    }

    fn start_apply(&mut self, plan: Plan) {
        let Some(view) = self.current() else { return };
        let inst = view.inst.clone();
        let paths = self.ctx.paths.clone();
        let config = self.ctx.config.clone();
        let egui = self.egui.clone();
        self.spawn("Applying changes", move |tx| {
            let (etx, erx) = mpsc::channel::<ops::Event>();
            let r = std::thread::scope(|s| {
                s.spawn(|| {
                    for e in erx {
                        let msg = match e {
                            ops::Event::Status(s) => TaskMsg::Status(s),
                            ops::Event::Download(DownloadEvent::Progress {
                                identifier,
                                bytes,
                                total,
                            }) => TaskMsg::Download {
                                id: identifier,
                                bytes,
                                total,
                            },
                            ops::Event::Download(DownloadEvent::Done { identifier }) => {
                                TaskMsg::DownloadDone(identifier)
                            }
                        };
                        let _ = tx.send(msg);
                        egui.request_repaint();
                    }
                });
                let r = ops::apply(&paths, &config, &inst, &plan, &etx);
                drop(etx);
                r
            });
            Done::Applied(r.map_err(|e| format!("{e:#}")))
        });
    }

    fn launch(&mut self) {
        let Some(view) = self.current() else { return };
        let mut inst = view.inst.clone();
        if self.running.iter().any(|(id, _)| *id == inst.id) {
            self.note(format!("{} is already running", inst.config.name));
            return;
        }
        let result = launch::plan(&inst, &self.steam, &self.ctx.config).and_then(|p| {
            log::info!("launch: {}", p.display());
            launch::spawn(&p)
        });
        match result {
            Ok(child) => {
                self.note(format!("Started {}", inst.config.name));
                inst.config.last_played = Some(orbis::paths::now_secs());
                if let Err(e) = inst.save() {
                    log::warn!("save last played: {e:#}");
                }
                self.running.push((inst.id.clone(), child));
                self.reload();
            }
            Err(e) => self.error(format!("Launch failed: {e:#}")),
        }
    }

    /// Fills the instance settings form from the current instance.
    pub fn sync_settings_form(&mut self) {
        let Some(view) = self.current() else { return };
        if self.settings.for_instance.as_ref() == Some(&view.inst.id) {
            return;
        }
        let c = &view.inst.config;
        self.settings = SettingsForm {
            for_instance: Some(view.inst.id.clone()),
            name: c.name.clone(),
            compat: c
                .compat
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            allow_testing: c.allow_testing,
            prefix_isolated: c.launch.prefix == PrefixMode::Instance,
            args: c.launch.args.join(" "),
            env: c
                .launch
                .env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("\n"),
            notes: c.notes.clone(),
        };
    }

    /// Fills the global settings form from the config.
    pub fn sync_global_form(&mut self) {
        if self.global.loaded {
            return;
        }
        let c = &self.ctx.config;
        let path = |p: &Option<PathBuf>| {
            p.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        self.global = GlobalForm {
            loaded: true,
            steam_root: path(&c.steam_root),
            proton: path(&c.proton),
            use_runtime: c.use_steam_runtime,
            workers: c.download_workers.to_string(),
            repos: c
                .repositories
                .iter()
                .map(|r| format!("{} {}", r.name, r.url))
                .collect::<Vec<_>>()
                .join("\n"),
        };
    }

    fn save_instance_settings(&mut self) {
        let Some(view) = self.current() else { return };
        let mut inst = view.inst.clone();
        let f = self.settings.clone();
        let mut compat = Vec::new();
        for v in f.compat.split([',', ' ']).filter(|s| !s.trim().is_empty()) {
            match GameVersion::parse(v) {
                Some(gv) if !gv.is_any() => compat.push(gv),
                _ => return self.error(format!("Not a game version: {v:?}")),
            }
        }
        let mut env = BTreeMap::new();
        for line in f.env.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match line.split_once('=') {
                Some((k, v)) if !k.trim().is_empty() => {
                    env.insert(k.trim().to_owned(), v.to_owned());
                }
                _ => return self.error(format!("Environment lines must be KEY=VALUE: {line:?}")),
            }
        }
        inst.config.name = f.name.trim().to_owned();
        inst.config.compat = compat;
        inst.config.allow_testing = f.allow_testing;
        inst.config.launch.prefix = if f.prefix_isolated {
            PrefixMode::Instance
        } else {
            PrefixMode::Steam
        };
        inst.config.launch.args = f.args.split_whitespace().map(str::to_owned).collect();
        inst.config.launch.env = env;
        inst.config.notes = f.notes;
        match inst.save() {
            Ok(()) => {
                self.note(format!("Saved settings for {}", inst.config.name));
                self.reload();
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn save_global_settings(&mut self) {
        let g = self.global.clone();
        let opt_path = |s: &str| {
            let s = s.trim();
            (!s.is_empty()).then(|| PathBuf::from(s))
        };
        let mut repos = Vec::new();
        for line in g.repos.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (name, url) = line
                .split_once(char::is_whitespace)
                .unwrap_or(("repo", line));
            repos.push(orbis::config::Repository {
                name: name.trim().to_owned(),
                url: url.trim().to_owned(),
            });
        }
        let Ok(workers) = g.workers.trim().parse() else {
            return self.error("Download workers must be a number".into());
        };
        let cfg = Config {
            steam_root: opt_path(&g.steam_root),
            proton: opt_path(&g.proton),
            use_steam_runtime: g.use_runtime,
            repositories: repos,
            download_workers: workers,
        };
        match cfg.save(&self.ctx.paths) {
            Ok(()) => {
                self.ctx.config = cfg;
                self.steam = self.ctx.steam();
                self.note("Saved global settings");
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    /// A text dump of the interface state, for headless verification.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        let mut line = |s: String| {
            out.push_str(&s);
            out.push('\n');
        };
        line(format!("index: {}", self.index_status));
        line(format!(
            "tab: {:?}  filter: {:?}  search: {:?}",
            self.tab, self.filter, self.search
        ));
        for v in &self.instances {
            let mark = if self.selected.as_ref() == Some(&v.inst.id) {
                '*'
            } else {
                ' '
            };
            line(format!(
                "{mark} instance {} ({}) KSP {} mods {}",
                v.inst.config.name,
                v.inst.id,
                v.inst.config.game_version,
                v.reg.mods.len()
            ));
        }
        line(format!("rows: {}", self.rows.len()));
        for r in self.rows.iter().take(25) {
            line(format!(
                "  {:40} installed={:12} latest={:12} ksp={}",
                r.id,
                r.installed.as_ref().map_or("-".into(), ToString::to_string),
                r.latest.as_ref().map_or("-".into(), ToString::to_string),
                r.game
            ));
        }
        line(format!(
            "queue: install={:?} remove={:?} upgrade={:?}",
            self.queue.install, self.queue.remove, self.queue.upgrade
        ));
        line(format!("selected mod: {:?}", self.selected_mod));
        line(format!(
            "manual library: {}",
            self.manual
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let dialog = match &self.dialog {
            None => "none".to_owned(),
            Some(Dialog::Error(e)) => format!("error: {e}"),
            Some(Dialog::Confirm { plan, .. }) => format!(
                "confirm: {}",
                plan.changes
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Some(Dialog::Choice { relationship, .. }) => format!("choice: {relationship}"),
            Some(_) => "other".into(),
        };
        line(format!("dialog: {dialog}"));
        line(format!("status: {}", self.last_message()));
        out
    }
}

fn index_label(index: &Index) -> String {
    let age = orbis::paths::now_secs().saturating_sub(index.updated);
    let when = match age {
        0..=119 => "just now".to_owned(),
        120..=7199 => format!("{} min ago", age / 60),
        7200..=172_799 => format!("{} h ago", age / 3600),
        _ => format!("{} days ago", age / 86400),
    };
    format!("{} mods · updated {when}", index.len())
}

fn add_options(form: &AddManualForm) -> AddOptions {
    let list = |s: &str| {
        s.split(',')
            .map(|x| x.trim().to_owned())
            .filter(|x| !x.is_empty())
            .collect()
    };
    AddOptions {
        path: PathBuf::from(form.path.trim()),
        name: form.name.clone(),
        version: form.version.clone(),
        provides: list(&form.provides),
        depends: list(&form.depends),
        conflicts: list(&form.conflicts),
        install: if form.find.trim().is_empty() {
            vec![]
        } else {
            vec![orbis::metadata::InstallDirective {
                find: Some(form.find.trim().to_owned()),
                install_to: "GameData".into(),
                ..Default::default()
            }]
        },
        source_url: form.url.clone(),
        notes: form.notes.clone(),
    }
}

/// The top-level folders an archive would install into.
fn preview_manual(form: &AddManualForm) -> Option<Result<Vec<String>, String>> {
    let path = PathBuf::from(form.path.trim());
    if form.path.trim().is_empty() {
        return None;
    }
    let result = (|| -> Result<Vec<String>> {
        let source = if path.is_dir() {
            orbis::install::Source::Dir(path.clone())
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        {
            orbis::install::Source::Zip(path.clone())
        } else {
            anyhow::bail!("pick a .zip file or a folder");
        };
        let entries = source.entries()?;
        let opts = add_options(form);
        let directives = if opts.install.is_empty() {
            manual::detect_directives(&entries, &form.name)
        } else {
            opts.install
        };
        let planned = orbis::install::plan(&directives, &entries)?;
        let mut tops: Vec<String> = planned
            .iter()
            .filter(|p| !p.is_dir)
            .map(|p| p.dest.splitn(3, '/').take(2).collect::<Vec<_>>().join("/"))
            .collect();
        let count = planned.iter().filter(|p| !p.is_dir).count();
        tops.sort();
        tops.dedup();
        tops.insert(0, format!("{count} files into:"));
        Ok(tops)
    })();
    Some(result.map_err(|e| format!("{e:#}")))
}

/// Opens a path or URL with the desktop's default handler.
fn open_external(target: &str) {
    if let Err(e) = std::process::Command::new("xdg-open")
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        log::warn!("xdg-open {target}: {e}");
    }
}

/// Paths of the instance's main folders, for "open folder" buttons.
pub fn instance_paths(view: &InstanceView) -> [(String, PathBuf); 3] {
    let g = view.inst.game_dir();
    [
        ("Game".into(), g.clone()),
        ("GameData".into(), g.join("GameData")),
        ("Saves".into(), g.join("saves")),
    ]
}

/// Paths for display in settings.
pub fn data_paths(paths: &Paths) -> [(String, PathBuf); 3] {
    [
        ("Instances".into(), paths.instances()),
        ("Manual mods".into(), paths.manual()),
        ("Download cache".into(), paths.downloads()),
    ]
}
