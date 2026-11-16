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

use orbis::cancel::Cancel;
use orbis::ckan_import;
use orbis::config::Config;
use orbis::download::DownloadEvent;
use orbis::game::Dlc;
use orbis::instance::{self, CopyMode, CreateOptions, Instance, ModSource, PrefixMode, Registry};
use orbis::launch;
use orbis::manual::{self, AddOptions, ManualMod};
use orbis::metadata::Module;
use orbis::modpack;
use orbis::ops;
use orbis::paths::Paths;
use orbis::repo::{self, Index};
use orbis::resolver::{Plan, Request, ResolveError};
use orbis::starred::Starred;
use orbis::steam::SteamInfo;
use orbis::version::{GameVersion, ModVersion};
use orbis::{backup, history, logs, store};

/// Height of one mod table row including spacing (see `view::ROW_H`).
pub const ROW_STEP: f32 = 30.0;
use serde::{Deserialize, Serialize};

use super::Options;
use crate::cli::{self, Context};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Mods,
    Manual,
    History,
    Logs,
    Settings,
}

/// Columns the mod table can sort by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Installed,
    Released,
    Downloads,
}

impl SortKey {
    fn name(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Installed => "installed",
            SortKey::Released => "released",
            SortKey::Downloads => "downloads",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        [
            SortKey::Name,
            SortKey::Installed,
            SortKey::Released,
            SortKey::Downloads,
        ]
        .into_iter()
        .find(|k| k.name() == s)
    }
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
    pub starred: bool,
    /// Display names of installed mods that depend on this one.
    pub required_by: Vec<String>,
    /// Release date of the shown release (YYYY-MM-DD), when known.
    pub released: String,
}

impl Row {
    pub fn upgradable(&self) -> bool {
        !self.manual && matches!((&self.installed, &self.latest), (Some(i), Some(l)) if l > i)
    }
}

/// Changes the user has queued but not applied.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
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

/// Interface state kept between runs in `<config>/gui.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Prefs {
    pub selected: Option<String>,
    pub filter: String,
    pub show_deps: bool,
    pub starred_only: bool,
    /// "<column>-asc" or "<column>-desc"; empty for each filter's default.
    pub sort: String,
    /// Vim-style keys are on unless this is set. (0.5.0 stored an opt-in
    /// `vim_keys` flag; it is ignored now that the keys are on by default.)
    pub vim_keys_off: bool,
    /// Unapplied changes, per instance id.
    pub queues: BTreeMap<String, Queue>,
}

impl Prefs {
    fn path(paths: &Paths) -> PathBuf {
        paths.config.join("gui.toml")
    }

    fn load(paths: &Paths) -> Self {
        std::fs::read_to_string(Self::path(paths))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        orbis::paths::write_atomic(&Self::path(paths), text.as_bytes())
    }
}

/// Fields of the export dialog.
#[derive(Clone, Debug, Default)]
pub struct ExportForm {
    pub instance: String,
    pub path: String,
    pub name: String,
    pub description: String,
    pub craft: bool,
    pub saves: Vec<(String, bool)>,
    pub manual_files: bool,
    pub ckan: bool,
}

/// Fields of the import dialog.
#[derive(Clone, Debug, Default)]
pub struct ImportForm {
    pub path: String,
    pub name: String,
    pub base: String,
    pub exact: bool,
    /// For a CKAN install: copy everything instead of linking stock files.
    pub full_copy: bool,
    /// The path is a CKAN-managed KSP install rather than a pack file.
    pub ckan_install: bool,
    /// A summary of the pack, or why it can't be read.
    pub preview: Option<Result<String, String>>,
    pub previewed_for: String,
}

/// Text buffers for the instance settings form.
#[derive(Clone, Debug, Default)]
pub struct SettingsForm {
    pub for_instance: Option<String>,
    pub name: String,
    pub compat: String,
    pub allow_testing: bool,
    pub prefix_isolated: bool,
    pub via_steam: bool,
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
    pub backups_keep: String,
    pub auto_update_hours: String,
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
    /// Set when adopting a hand-added GameData folder of the current
    /// instance instead of adding an archive.
    pub adopt_folder: Option<String>,
}

/// A modal dialog.
pub enum Dialog {
    NewInstance {
        name: String,
        source: String,
        with_saves: bool,
        full_copy: bool,
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
    RestoreHistory {
        id: u64,
        before: bool,
        title: String,
        lines: Vec<String>,
    },
    RestoreBackup {
        id: String,
        save: Option<String>,
        title: String,
    },
    Export(Box<ExportForm>),
    Import(Box<ImportForm>),
    DeleteManual(ManualMod),
    RenameInstance {
        id: String,
        name: String,
    },
    KeyHelp,
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
    ToggleStar(String),
    SortBy(SortKey),
    SetVimKeys(bool),
    CancelTask,
    OpenRename,
    RenameInstance,
    CleanStore,
    ShareStore,
    RefreshStorage,
    AskRestoreHistory(u64, bool),
    RestoreHistory,
    BackupNow,
    AskRestoreBackup(String, Option<String>),
    RestoreBackup,
    LoadLog(logs::LogKind),
    SetLogLevel(logs::Level),
    JumpToLog(usize),
    OpenAdopt(String),
    SetShowDeps(bool),
    SetStarredOnly(bool),
    QueueShown,
    SetAuto(String, bool),
    OpenExport,
    Export,
    OpenImport(Option<PathBuf>),
    PreviewImport,
    Import,
    CloseDialog,
    Refresh,
}

/// Results of background work.
pub enum Done {
    IndexLoaded(Result<Option<Index>, String>),
    IndexUpdated(Result<(repo::UpdateSummary, Index), String>),
    /// The instance the changes were applied to, and the outcome.
    Applied(String, Result<(), String>),
    InstanceCreated(Result<Instance, String>),
    ManualAdded(Result<ManualMod, String>),
    Exported(Result<(PathBuf, Vec<String>), String>),
    Imported(Result<modpack::ImportReport, String>),
    Restored(Result<Vec<String>, String>),
    CkanImported(Result<ckan_import::Report, String>),
    Shared(Result<store::Shared, String>),
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
    /// Whether the work checks for cancellation.
    pub cancellable: bool,
    pub cancel: Cancel,
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
    rows_key: Option<(String, Filter, String, u64, usize, bool, bool)>,
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
    pub starred: Starred,
    pub show_deps: bool,
    pub starred_only: bool,
    /// Dependencies hidden from the Installed list.
    pub hidden_deps: usize,
    queues: BTreeMap<String, Queue>,
    /// `None` uses the filter's default order.
    pub sort: Option<(SortKey, bool)>,
    /// Upgrades waiting per instance id.
    pub upgrade_counts: HashMap<String, usize>,
    pub history: Vec<history::Entry>,
    pub backups: Vec<backup::Backup>,
    pub unmanaged: (Vec<manual::Unmanaged>, Vec<String>),
    pub log_kind: logs::LogKind,
    pub log: Option<Result<logs::Report, String>>,
    pub log_min: logs::Level,
    pub log_search: String,
    /// Indexes into the log's lines that pass the level and search filters.
    pub log_rows: Vec<usize>,
    log_rows_key: Option<(logs::Level, String, usize)>,
    pub log_jump: Option<usize>,
    auto_updated: bool,
    pub vim_keys: bool,
    /// A `g` waiting for its second key (gg, gt, gT), with the time it came.
    pending_g: Option<f64>,
    /// Scroll the mod table so this row is visible.
    pub scroll_to_row: Option<usize>,
    /// The mod table's scroll offset and visible height, from the last frame.
    pub mods_view: (f32, f32),
    pub focus_search: bool,
    pub storage: Option<Result<store::Usage, String>>,
    /// KSP's launch options in Steam, as last read.
    pub steam_launch_options: Option<String>,
    saved_prefs: Prefs,
    /// Write `gui.toml` (off in headless mode).
    persist: bool,
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
            starred: Starred::default(),
            show_deps: false,
            starred_only: false,
            hidden_deps: 0,
            queues: BTreeMap::new(),
            sort: None,
            upgrade_counts: HashMap::new(),
            history: Vec::new(),
            backups: Vec::new(),
            unmanaged: (Vec::new(), Vec::new()),
            log_kind: logs::LogKind::Ksp,
            log: None,
            log_min: logs::Level::Info,
            log_search: String::new(),
            log_rows: Vec::new(),
            log_rows_key: None,
            log_jump: None,
            auto_updated: false,
            vim_keys: true,
            pending_g: None,
            scroll_to_row: None,
            mods_view: (0.0, 0.0),
            focus_search: false,
            storage: None,
            steam_launch_options: None,
            saved_prefs: Prefs::default(),
            persist: !opts.headless,
            generation: 0,
        };
        match Starred::load(&app.ctx.paths) {
            Ok(s) => app.starred = s,
            Err(e) => app.messages.push(format!("{e:#}")),
        }
        app.steam_launch_options = launch::current_steam_launch_options(&app.steam);
        let prefs = Prefs::load(&app.ctx.paths);
        app.selected = prefs.selected.clone();
        app.filter = filter_from(&prefs.filter).unwrap_or(Filter::Installed);
        app.show_deps = prefs.show_deps;
        app.starred_only = prefs.starred_only;
        app.vim_keys = !prefs.vim_keys_off;
        app.sort = prefs
            .sort
            .split_once('-')
            .and_then(|(k, d)| SortKey::from_name(k).map(|k| (k, d == "desc")));
        app.queues = prefs.queues.clone();
        app.saved_prefs = prefs;
        app.reload();
        // Pick the starting instance before taking its unapplied changes.
        if let Some(key) = &opts.instance
            && let Some(v) = app
                .instances
                .iter()
                .find(|v| &v.inst.id == key || v.inst.config.name.eq_ignore_ascii_case(key))
        {
            app.selected = Some(v.inst.id.clone());
        }
        if let Some(sel) = &app.selected {
            app.queue = app.queues.remove(sel).unwrap_or_default();
        }
        app.tab = match opts.tab.as_deref() {
            Some("manual") => Tab::Manual,
            Some("history") => Tab::History,
            Some("logs") => Tab::Logs,
            Some("settings") => Tab::Settings,
            _ => Tab::Mods,
        };
        app.load_side_data();
        if app.tab == Tab::Logs {
            app.update(Msg::LoadLog(logs::LogKind::Ksp));
        }
        if let Some(f) = opts.filter.as_deref().and_then(filter_from) {
            app.filter = f;
        }
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
        self.load_side_data();
        self.recount_upgrades();
    }

    /// Reads the current instance's history, backups and unmanaged files.
    fn load_side_data(&mut self) {
        let Some(view) = self.current() else {
            self.history.clear();
            self.backups.clear();
            self.unmanaged = (vec![], vec![]);
            return;
        };
        let inst = view.inst.clone();
        let owners = view.reg.owners();
        self.history = history::list(&inst).unwrap_or_else(|e| {
            log::warn!("history: {e:#}");
            vec![]
        });
        self.backups = backup::list(&inst).unwrap_or_else(|e| {
            log::warn!("backups: {e:#}");
            vec![]
        });
        self.unmanaged = manual::unmanaged(&inst.game_dir(), &owners).unwrap_or_else(|e| {
            log::warn!("unmanaged: {e:#}");
            (vec![], vec![])
        });
    }

    /// Counts the upgrades waiting in every instance.
    fn recount_upgrades(&mut self) {
        let Some(index) = self.index.clone() else {
            return;
        };
        self.upgrade_counts = self
            .instances
            .iter()
            .map(|v| {
                (
                    v.inst.id.clone(),
                    ops::upgradable(&v.inst, &index, &v.reg).len(),
                )
            })
            .collect();
    }

    /// Starts an index update when the cached one is missing or stale.
    fn maybe_auto_update(&mut self) {
        let hours = self.ctx.config.auto_update_hours;
        if !self.persist || self.auto_updated || hours == 0 {
            return;
        }
        let stale = self
            .index
            .as_ref()
            .is_none_or(|i| orbis::paths::now_secs().saturating_sub(i.updated) > hours * 3600);
        if stale {
            self.auto_updated = true;
            self.note("Mod index is out of date; updating");
            self.update(Msg::UpdateIndex);
        }
    }

    pub fn current(&self) -> Option<&InstanceView> {
        let id = self.selected.as_ref()?;
        self.instances.iter().find(|v| &v.inst.id == id)
    }

    fn error(&mut self, msg: String) {
        if msg.contains(CANCELLED) {
            self.note("Cancelled");
            return;
        }
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
        self.handle_keys(ui.ctx());
        self.ensure_rows();
        let mut msgs = Vec::new();
        super::view::draw(self, ui, &mut msgs);
        for m in msgs {
            self.update(m);
        }
        self.save_prefs();
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
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if ext == "orbispack" || ext == "ckan" || ckan_import::is_ckan_install(&p) {
                self.update(Msg::OpenImport(Some(p)));
            } else {
                self.tab = Tab::Manual;
                self.update(Msg::OpenAddManual(Some(p)));
            }
        }
    }

    // ---- keys -----------------------------------------------------------

    /// Keyboard shortcuts. Standard keys (arrows, Page Up/Down, Ctrl
    /// combinations, F-keys) always work; letter keys work when vim keys are
    /// on. Nothing fires while a text field has focus or a dialog is open.
    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() || ctx.egui_wants_keyboard_input() {
            return;
        }
        let (events, now) = ctx.input(|i| (i.events.clone(), i.time));
        for e in events {
            match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => self.standard_key(key, modifiers),
                egui::Event::Text(t) => {
                    for c in t.chars() {
                        if c == '?' {
                            self.dialog = Some(Dialog::KeyHelp);
                        } else if self.vim_keys {
                            self.vim_char(c, now);
                        }
                    }
                }
                _ => {}
            }
            if self.dialog.is_some() {
                break;
            }
        }
    }

    fn standard_key(&mut self, key: egui::Key, m: egui::Modifiers) {
        use egui::Key;
        let on_mods = self.tab == Tab::Mods;
        let page = (self.mods_view.1 / ROW_STEP).max(1.0) as isize;
        let tabs = [
            Tab::Mods,
            Tab::Manual,
            Tab::History,
            Tab::Logs,
            Tab::Settings,
        ];
        match key {
            Key::ArrowDown if m.ctrl => self.cycle_instance(1),
            Key::ArrowUp if m.ctrl => self.cycle_instance(-1),
            Key::Tab if m.ctrl => self.cycle_tab(if m.shift { -1 } else { 1 }),
            Key::Num1 | Key::Num2 | Key::Num3 | Key::Num4 | Key::Num5 if m.ctrl => {
                let i = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5]
                    .iter()
                    .position(|k| *k == key)
                    .unwrap_or(0);
                self.update(Msg::SetTab(tabs[i]));
            }
            Key::F if m.ctrl => {
                self.tab = Tab::Mods;
                self.focus_search = true;
            }
            Key::Z if m.ctrl => match self.history.first() {
                Some(e) => {
                    let id = e.id;
                    self.update(Msg::AskRestoreHistory(id, true));
                }
                None => self.note("Nothing to undo"),
            },
            // Vim's half-page moves; Ctrl+D and Ctrl+U mean nothing else here.
            Key::D if m.ctrl && self.vim_keys && on_mods => self.move_selection(page / 2),
            Key::U if m.ctrl && self.vim_keys && on_mods => self.move_selection(-(page / 2)),
            Key::F1 => self.dialog = Some(Dialog::KeyHelp),
            Key::F2 => self.update(Msg::OpenRename),
            Key::F5 => {
                self.update(Msg::Refresh);
                self.note("Re-read instances from disk");
            }
            _ if m.ctrl || m.alt => {}
            Key::ArrowDown if on_mods => self.move_selection(1),
            Key::ArrowUp if on_mods => self.move_selection(-1),
            Key::PageDown if on_mods => self.move_selection(page),
            Key::PageUp if on_mods => self.move_selection(-page),
            Key::Home if on_mods => self.select_row(0),
            Key::End if on_mods => self.select_row(self.rows.len().saturating_sub(1)),
            Key::ArrowLeft if on_mods => self.cycle_filter(-1),
            Key::ArrowRight if on_mods => self.cycle_filter(1),
            Key::Space if on_mods => self.toggle_selected(),
            Key::Delete if on_mods => {
                if let Some(row) = self.selected_row().cloned()
                    && row.installed.is_some()
                    && !row.dlc
                    && !self.queue.remove.contains(&row.id)
                {
                    self.update(Msg::ToggleRemove(row.id));
                }
            }
            Key::Enter if !self.queue.is_empty() => self.update(Msg::ApplyQueue),
            _ => {}
        }
    }

    /// Ticks or unticks the selected mod, like clicking its checkbox.
    fn toggle_selected(&mut self) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        if row.dlc {
            return;
        }
        if row.installed.is_some() {
            self.update(Msg::ToggleRemove(row.id));
        } else if row.latest.is_some() {
            self.update(Msg::ToggleInstall(row.id));
        }
    }

    fn vim_char(&mut self, c: char, now: f64) {
        let pending = self.pending_g.take().is_some_and(|t| now - t < 1.0);
        if pending {
            match c {
                'g' => self.select_row(0),
                't' => self.cycle_tab(1),
                'T' => self.cycle_tab(-1),
                _ => {}
            }
            return;
        }
        let on_mods = self.tab == Tab::Mods;
        match c {
            'g' => self.pending_g = Some(now),
            'J' => self.cycle_instance(1),
            'K' => self.cycle_instance(-1),
            'j' if on_mods => self.move_selection(1),
            'k' if on_mods => self.move_selection(-1),
            'G' if on_mods => self.select_row(self.rows.len().saturating_sub(1)),
            'h' if on_mods => self.cycle_filter(-1),
            'l' if on_mods => self.cycle_filter(1),
            '/' if on_mods => self.focus_search = true,
            'x' if on_mods => self.toggle_selected(),
            'u' if on_mods => {
                if let Some(row) = self.selected_row().filter(|r| r.upgradable()).cloned() {
                    self.update(Msg::ToggleUpgrade(row.id));
                }
            }
            's' if on_mods => {
                if let Some(row) = self.selected_row().filter(|r| !r.dlc).cloned() {
                    self.update(Msg::ToggleStar(row.id));
                }
            }
            _ => {}
        }
    }

    fn selected_row(&self) -> Option<&Row> {
        let id = self.selected_mod.as_ref()?;
        self.rows.iter().find(|r| &r.id == id)
    }

    fn select_row(&mut self, i: usize) {
        if let Some(row) = self.rows.get(i) {
            self.selected_mod = Some(row.id.clone());
            self.scroll_to_row = Some(i);
        }
    }

    fn move_selection(&mut self, by: isize) {
        if self.rows.is_empty() {
            return;
        }
        let current = self
            .selected_mod
            .as_ref()
            .and_then(|id| self.rows.iter().position(|r| &r.id == id));
        let next = match current {
            Some(i) => i.saturating_add_signed(by).min(self.rows.len() - 1),
            None => 0,
        };
        self.select_row(next);
    }

    fn cycle_filter(&mut self, by: isize) {
        let all = Filter::ALL;
        let i = all.iter().position(|f| *f == self.filter).unwrap_or(0);
        let next = (i as isize + by).rem_euclid(all.len() as isize) as usize;
        self.update(Msg::SetFilter(all[next]));
    }

    fn cycle_tab(&mut self, by: isize) {
        let all = [
            Tab::Mods,
            Tab::Manual,
            Tab::History,
            Tab::Logs,
            Tab::Settings,
        ];
        let i = all.iter().position(|t| *t == self.tab).unwrap_or(0);
        let next = (i as isize + by).rem_euclid(all.len() as isize) as usize;
        self.update(Msg::SetTab(all[next]));
    }

    fn cycle_instance(&mut self, by: isize) {
        if self.instances.is_empty() {
            return;
        }
        let i = self
            .selected
            .as_ref()
            .and_then(|s| self.instances.iter().position(|v| &v.inst.id == s))
            .unwrap_or(0);
        let next = (i as isize + by).rem_euclid(self.instances.len() as isize) as usize;
        let id = self.instances[next].inst.id.clone();
        self.update(Msg::SelectInstance(id));
    }

    // ---- background work --------------------------------------------

    /// Runs work that can't be stopped part-way.
    fn spawn(&mut self, title: &str, work: impl FnOnce(&Sender<TaskMsg>) -> Done + Send + 'static) {
        self.spawn_task(title, false, move |tx, _| work(tx));
    }

    /// Runs work that obeys the Cancel button.
    fn spawn_cancellable(
        &mut self,
        title: &str,
        work: impl FnOnce(&Sender<TaskMsg>, &Cancel) -> Done + Send + 'static,
    ) {
        self.spawn_task(title, true, work);
    }

    fn spawn_task(
        &mut self,
        title: &str,
        cancellable: bool,
        work: impl FnOnce(&Sender<TaskMsg>, &Cancel) -> Done + Send + 'static,
    ) {
        if self.task.is_some() {
            self.note("Another task is still running.");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let egui = self.egui.clone();
        let cancel = Cancel::default();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            let done = work(&tx, &flag);
            let _ = tx.send(TaskMsg::Done(Box::new(done)));
            egui.request_repaint();
        });
        self.task = Some(Task {
            title: title.to_owned(),
            status: String::new(),
            progress: None,
            downloads: BTreeMap::new(),
            cancellable,
            cancel,
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
                self.recount_upgrades();
                self.maybe_auto_update();
            }
            Done::IndexLoaded(Ok(None)) => {
                self.index_status = "No mod index yet: press Update".into();
                self.maybe_auto_update();
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
                self.recount_upgrades();
            }
            Done::IndexUpdated(Err(e)) => self.error(format!("Update failed: {e}")),
            Done::CkanImported(r) => match r {
                Ok(report) => {
                    self.note(format!(
                        "Imported {} mods into {}",
                        report.imported, report.instance.config.name
                    ));
                    self.selected = Some(report.instance.id.clone());
                    self.queue = Queue::default();
                    self.reload();
                    let mut notes = report.warnings.clone();
                    if !report.unmanaged.is_empty() {
                        notes.push(format!(
                            "CKAN didn't track these GameData folders; adopt them on the Manual mods tab: {}",
                            report.unmanaged.join(", ")
                        ));
                    }
                    if !notes.is_empty() {
                        self.error(format!("Imported, with notes:\n{}", notes.join("\n")));
                    }
                }
                Err(e) => {
                    self.reload();
                    self.error(format!("Import failed: {e}"));
                }
            },
            Done::Shared(r) => {
                match r {
                    Ok(s) => {
                        self.note(format!(
                            "{} files ({:.1} MiB) now share disk space with the store",
                            s.files,
                            s.bytes as f64 / 1_048_576.0
                        ));
                        if !s.skipped.is_empty() {
                            self.error(format!(
                                "Some mods were skipped because their downloads aren't cached (reinstalling them shares space too):\n{}",
                                s.skipped.join("\n")
                            ));
                        }
                    }
                    Err(e) => self.error(format!("Sharing disk space failed: {e}")),
                }
                self.update(Msg::RefreshStorage);
            }
            Done::Restored(r) => {
                self.reload();
                match r {
                    Ok(w) if w.is_empty() => self.note("Mod list restored"),
                    Ok(w) => self.error(format!("Restored with warnings:\n{}", w.join("\n"))),
                    Err(e) => self.error(format!("Restore failed: {e}")),
                }
            }
            Done::Applied(id, r) => {
                self.reload();
                match r {
                    Ok(()) => {
                        // The user may have switched instances meanwhile:
                        // clear the queue of the instance that was changed.
                        if self.selected.as_ref() == Some(&id) {
                            self.queue = Queue::default();
                            self.choices.clear();
                        } else {
                            self.queues.remove(&id);
                        }
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
            Done::Exported(r) => match r {
                Ok((path, warnings)) => {
                    self.note(format!("Exported {}", path.display()));
                    if !warnings.is_empty() {
                        self.error(format!("Exported with warnings:\n{}", warnings.join("\n")));
                    }
                }
                Err(e) => self.error(format!("Export failed: {e}")),
            },
            Done::Imported(r) => match r {
                Ok(report) => {
                    self.note(format!("Imported {}", report.instance.config.name));
                    self.selected = Some(report.instance.id.clone());
                    self.queue = Queue::default();
                    self.reload();
                    if !report.warnings.is_empty() {
                        self.error(format!(
                            "Imported with warnings:\n{}",
                            report.warnings.join("\n")
                        ));
                    }
                }
                Err(e) => {
                    self.reload();
                    self.error(format!("Import failed: {e}"));
                }
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
            self.show_deps,
            self.starred_only,
        );
        if self.rows_key.as_ref() == Some(&key) {
            return;
        }
        let (rows, hidden) = self.build_rows(&key.2);
        self.rows = rows;
        self.hidden_deps = hidden;
        self.rows_key = Some(key);
    }

    /// Rebuilds the rows on the next frame.
    fn invalidate_rows(&mut self) {
        self.rows_key = None;
    }

    /// The rows for the current filter, and how many dependencies the
    /// Installed list hid.
    fn build_rows(&self, query: &str) -> (Vec<Row>, usize) {
        let Some(view) = self.current() else {
            return (vec![], 0);
        };
        let compat = view.inst.compat_versions();
        let testing = view.inst.config.allow_testing;
        let empty = Index::default();
        let index: &Index = self.index.as_deref().unwrap_or(&empty);
        let name_of = |id: &str| {
            view.reg
                .mods
                .get(id)
                .map_or(id.to_owned(), |m| m.module.display_name().to_owned())
        };
        let mut rows = Vec::new();
        let mut hidden = 0;
        if matches!(self.filter, Filter::Installed | Filter::Upgradable) {
            for (id, m) in &view.reg.mods {
                if !cli::matches(&m.module, query) {
                    continue;
                }
                let latest = index
                    .latest_compatible(id, &compat, testing)
                    .map(|l| l.version.clone());
                let mut required_by: Vec<String> = view
                    .reg
                    .required_by(id)
                    .iter()
                    .map(|r| name_of(r))
                    .collect();
                required_by.sort_by_key(|n| n.to_lowercase());
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
                    starred: self.starred.contains(id),
                    required_by,
                    released: date(m.module.release_date.as_deref()),
                });
            }
            if self.filter == Filter::Installed {
                for d in &view.dlc {
                    if query.is_empty() || d.identifier.to_lowercase().contains(query) {
                        rows.push(Row {
                            id: d.identifier.clone(),
                            name: d.identifier.clone(),
                            summary: "Expansion (detected)".into(),
                            installed: Some(
                                d.version.clone().unwrap_or_else(|| ModVersion::new("?")),
                            ),
                            latest: None,
                            game: String::new(),
                            downloads: 0,
                            manual: false,
                            auto: false,
                            dlc: true,
                            starred: false,
                            required_by: vec![],
                            released: String::new(),
                        });
                    }
                }
                if !self.show_deps {
                    let before = rows.len();
                    rows.retain(|r| !r.auto);
                    hidden = before - rows.len();
                }
            } else {
                rows.retain(Row::upgradable);
            }
            if self.starred_only {
                rows.retain(|r| r.starred);
            }
            rows.sort_by_key(|r| r.name.to_lowercase());
            self.apply_sort(&mut rows);
            return (rows, hidden);
        }
        for (id, versions) in index.iter() {
            if self.starred_only && !self.starred.contains(id) {
                continue;
            }
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
                starred: self.starred.contains(id),
                required_by: installed
                    .map(|_| {
                        view.reg
                            .required_by(id)
                            .iter()
                            .map(|r| name_of(r))
                            .collect()
                    })
                    .unwrap_or_default(),
                released: date(shown.release_date.as_deref()),
            });
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.downloads));
        self.apply_sort(&mut rows);
        (rows, 0)
    }

    fn apply_sort(&self, rows: &mut [Row]) {
        let Some((key, desc)) = self.sort else { return };
        match key {
            SortKey::Name => rows.sort_by_key(|r| r.name.to_lowercase()),
            SortKey::Installed => {
                rows.sort_by_key(|r| (r.installed.is_none(), r.name.to_lowercase()))
            }
            SortKey::Released => rows.sort_by(|a, b| a.released.cmp(&b.released)),
            SortKey::Downloads => rows.sort_by_key(|r| r.downloads),
        }
        if desc {
            rows.reverse();
        }
    }

    /// Log line indexes that pass the level and search filters.
    pub fn ensure_log_rows(&mut self) {
        let Some(Ok(report)) = &self.log else {
            self.log_rows.clear();
            return;
        };
        let key = (
            self.log_min,
            self.log_search.to_lowercase(),
            report.lines.len(),
        );
        if self.log_rows_key.as_ref() == Some(&key) {
            return;
        }
        self.log_rows = report
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.level >= key.0 && (key.1.is_empty() || l.text.to_lowercase().contains(&key.1))
            })
            .map(|(i, _)| i)
            .collect();
        self.log_rows_key = Some(key);
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
                    if let Some(old) = self.selected.take() {
                        let q = std::mem::take(&mut self.queue);
                        if !q.is_empty() {
                            self.queues.insert(old, q);
                        }
                    }
                    self.queue = self.queues.remove(&id).unwrap_or_default();
                    self.selected = Some(id);
                    self.load_side_data();
                    self.log = None;
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
                self.spawn_cancellable("Updating mod index", move |tx, cancel| {
                    let mut report = |s: String| {
                        let _ = tx.send(TaskMsg::Status(s));
                    };
                    let r = repo::update(&paths, &config, &mut report, cancel).and_then(|s| {
                        let index = Index::load(&paths)?.unwrap_or_default();
                        Ok((s, index))
                    });
                    Done::IndexUpdated(r.map_err(task_error))
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
                    full_copy: false,
                    keep_mods: false,
                });
            }
            Msg::CreateInstance => {
                if let Some(Dialog::NewInstance {
                    name,
                    source,
                    with_saves,
                    full_copy,
                    keep_mods,
                }) = self.dialog.take()
                {
                    let paths = self.ctx.paths.clone();
                    let opts = CreateOptions {
                        name,
                        source: PathBuf::from(source.trim()),
                        stock_only: !(full_copy && keep_mods),
                        with_saves,
                        mode: if full_copy {
                            CopyMode::Copy
                        } else {
                            CopyMode::Link
                        },
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
                if let Some(Dialog::AddManual(form)) = &mut self.dialog
                    && form.adopt_folder.is_none()
                {
                    form.preview = preview_manual(form);
                    form.previewed_for = format!("{}|{}", form.path, form.find);
                }
            }
            Msg::AddManual => {
                if let Some(Dialog::AddManual(form)) = &self.dialog
                    && let Some(folder) = form.adopt_folder.clone()
                {
                    let opts = add_options(form);
                    self.dialog = None;
                    let Some(view) = self.current() else { return };
                    let inst = view.inst.clone();
                    match ops::adopt(&self.ctx.paths, &inst, &folder, &opts) {
                        Ok(m) => {
                            self.note(format!("Adopted GameData/{folder} as {}", m.name));
                            self.reload();
                        }
                        Err(e) => self.error(format!("{e:#}")),
                    }
                } else if let Some(Dialog::AddManual(form)) = self.dialog.take() {
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
            Msg::ToggleStar(id) => {
                self.starred.toggle(&id);
                if let Err(e) = self.starred.save(&self.ctx.paths) {
                    self.error(format!("{e:#}"));
                }
                self.generation += 1;
            }
            Msg::SetShowDeps(v) => self.show_deps = v,
            Msg::SetVimKeys(v) => self.vim_keys = v,
            Msg::CancelTask => {
                if let Some(t) = &mut self.task
                    && t.cancellable
                {
                    t.cancel.cancel();
                    t.status = "Cancelling (finishing the current step)...".into();
                }
            }
            Msg::OpenRename => {
                if let Some(v) = self.current() {
                    self.dialog = Some(Dialog::RenameInstance {
                        id: v.inst.id.clone(),
                        name: v.inst.config.name.clone(),
                    });
                }
            }
            Msg::RenameInstance => {
                if let Some(Dialog::RenameInstance { id, name }) = self.dialog.take()
                    && let Some(v) = self.instances.iter().find(|v| v.inst.id == id)
                {
                    let mut inst = v.inst.clone();
                    inst.config.name = name.trim().to_owned();
                    match inst.save() {
                        Ok(()) => {
                            self.note(format!("Renamed to {}", inst.config.name));
                            self.reload();
                        }
                        Err(e) => self.error(format!("{e:#}")),
                    }
                }
            }
            Msg::ShareStore => {
                let paths = self.ctx.paths.clone();
                self.spawn_cancellable("Sharing disk space", move |tx, cancel| {
                    let mut report = |m: String| {
                        let _ = tx.send(TaskMsg::Status(m));
                    };
                    Done::Shared(store::share(&paths, cancel, &mut report).map_err(task_error))
                });
            }
            Msg::RefreshStorage => {
                self.storage = Some(store::usage(&self.ctx.paths).map_err(|e| format!("{e:#}")));
            }
            Msg::CleanStore => {
                match store::clean(&self.ctx.paths) {
                    Ok((n, bytes)) => self.note(format!(
                        "Removed {n} unused entries ({:.1} MiB)",
                        bytes as f64 / 1_048_576.0
                    )),
                    Err(e) => self.error(format!("{e:#}")),
                }
                self.update(Msg::RefreshStorage);
            }
            Msg::SortBy(key) => {
                self.sort = match self.sort {
                    Some((k, desc)) if k == key => Some((key, !desc)),
                    _ => Some((key, matches!(key, SortKey::Released | SortKey::Downloads))),
                };
                self.invalidate_rows();
            }
            Msg::AskRestoreHistory(id, before) => self.ask_restore_history(id, before),
            Msg::RestoreHistory => {
                let Some(Dialog::RestoreHistory { id, before, .. }) = self.dialog.take() else {
                    return;
                };
                let (Some(view), Some(index)) = (self.current(), self.index.clone()) else {
                    return self
                        .error("The mod index isn't loaded yet: press Update index.".into());
                };
                let Some(entry) = self.history.iter().find(|e| e.id == id) else {
                    return;
                };
                let target = if before {
                    entry.before.clone()
                } else {
                    entry.after.clone()
                };
                let inst = view.inst.clone();
                let paths = self.ctx.paths.clone();
                let config = self.ctx.config.clone();
                let egui = self.egui.clone();
                self.spawn_cancellable("Restoring mod list", move |tx, cancel| {
                    let r = forward_events(tx, &egui, |etx| {
                        ops::restore(&paths, &config, &inst, &index, &target, etx, cancel)
                    });
                    Done::Restored(r.map_err(task_error))
                });
            }
            Msg::BackupNow => {
                if let Some(view) = self.current() {
                    let inst = view.inst.clone();
                    match backup::create(&inst, "manual") {
                        Ok(Some(b)) => self.note(format!("Backed up {}", b.saves.join(", "))),
                        Ok(None) => self.note(
                            "Nothing to back up: no saves, or none changed since the last backup",
                        ),
                        Err(e) => self.error(format!("{e:#}")),
                    }
                    self.load_side_data();
                }
            }
            Msg::AskRestoreBackup(id, save) => {
                if let Some(b) = self.backups.iter().find(|b| b.id == id) {
                    let what = save.clone().unwrap_or_else(|| b.saves.join(", "));
                    self.dialog = Some(Dialog::RestoreBackup {
                        title: format!(
                            "Restore {what} from the {} backup of {}?",
                            b.reason,
                            when(b.created)
                        ),
                        id,
                        save,
                    });
                }
            }
            Msg::RestoreBackup => {
                if let Some(Dialog::RestoreBackup { id, save, .. }) = self.dialog.take()
                    && let Some(view) = self.current()
                {
                    let inst = view.inst.clone();
                    match backup::restore(&inst, &id, save.as_deref()) {
                        Ok(()) => {
                            self.note("Saves restored (the previous saves were backed up first)")
                        }
                        Err(e) => self.error(format!("{e:#}")),
                    }
                    self.load_side_data();
                }
            }
            Msg::LoadLog(kind) => {
                self.log_kind = kind;
                self.log_rows_key = None;
                self.log_jump = None;
                if let Some(view) = self.current() {
                    let inst = view.inst.clone();
                    self.log = Some(match logs::path(&inst, &self.steam, kind) {
                        None => {
                            Err("This log's location is unknown (no Proton prefix found).".into())
                        }
                        Some(p) if !p.is_file() => Err(format!(
                            "{} doesn't exist yet: launch the game first.",
                            p.display()
                        )),
                        Some(p) => logs::assemblies(&inst)
                            .and_then(|asm| logs::read(&p, &asm))
                            .map_err(|e| format!("{e:#}")),
                    });
                }
            }
            Msg::SetLogLevel(l) => self.log_min = l,
            Msg::JumpToLog(line) => {
                self.log_min = logs::Level::Info;
                self.log_search.clear();
                self.log_jump = Some(line);
            }
            Msg::OpenAdopt(folder) => {
                let form = AddManualForm {
                    name: folder.clone(),
                    version: "1.0".into(),
                    preview: Some(Ok(vec![format!(
                        "Tracks GameData/{folder} as it is; nothing is copied into the game."
                    )])),
                    adopt_folder: Some(folder),
                    ..AddManualForm::default()
                };
                self.dialog = Some(Dialog::AddManual(Box::new(form)));
            }
            Msg::SetStarredOnly(v) => self.starred_only = v,
            Msg::QueueShown => {
                let ids: Vec<String> = self
                    .rows
                    .iter()
                    .filter(|r| r.installed.is_none() && r.latest.is_some())
                    .map(|r| r.id.clone())
                    .collect();
                self.note(format!("Queued {} mods", ids.len()));
                self.queue.install.extend(ids);
            }
            Msg::SetAuto(id, auto) => {
                if let Some(view) = self.current() {
                    let inst = view.inst.clone();
                    let result = inst.registry().and_then(|mut reg| {
                        if let Some(m) = reg.mods.get_mut(&id) {
                            m.auto_installed = auto;
                        }
                        inst.save_registry(&reg)
                    });
                    match result {
                        Ok(()) => self.reload(),
                        Err(e) => self.error(format!("{e:#}")),
                    }
                }
            }
            Msg::OpenExport => {
                if let Some(v) = self.current() {
                    let dir = orbis::paths::home().unwrap_or_default();
                    let file = format!("{}.orbispack", orbis::paths::slug(&v.inst.config.name));
                    self.dialog = Some(Dialog::Export(Box::new(ExportForm {
                        instance: v.inst.id.clone(),
                        path: dir.join(file).display().to_string(),
                        name: v.inst.config.name.clone(),
                        saves: modpack::save_names(&v.inst)
                            .into_iter()
                            .map(|n| (n, false))
                            .collect(),
                        ..ExportForm::default()
                    })));
                }
            }
            Msg::Export => {
                if let Some(Dialog::Export(form)) = self.dialog.take()
                    && let Some(v) = self.instances.iter().find(|v| v.inst.id == form.instance)
                {
                    let inst = v.inst.clone();
                    let paths = self.ctx.paths.clone();
                    let mut out = PathBuf::from(form.path.trim());
                    if form.ckan && out.extension().is_none_or(|e| e != "ckan") {
                        out.set_extension("ckan");
                    }
                    self.spawn("Exporting modpack", move |_| {
                        let r = if form.ckan {
                            modpack::export_ckan(&inst, &out, &form.name).map(|()| vec![])
                        } else {
                            let opts = modpack::ExportOptions {
                                out: out.clone(),
                                name: form.name.clone(),
                                description: form.description.clone(),
                                craft: form.craft,
                                saves: form
                                    .saves
                                    .iter()
                                    .filter(|(_, on)| *on)
                                    .map(|(n, _)| n.clone())
                                    .collect(),
                                manual_files: form.manual_files,
                            };
                            modpack::export(&paths, &inst, &opts)
                        };
                        Done::Exported(r.map(|w| (out, w)).map_err(|e| format!("{e:#}")))
                    });
                }
            }
            Msg::OpenImport(path) => {
                let form = ImportForm {
                    path: path.map(|p| p.display().to_string()).unwrap_or_default(),
                    base: self
                        .steam
                        .ksp
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    exact: true,
                    ..ImportForm::default()
                };
                self.dialog = Some(Dialog::Import(Box::new(form)));
                self.update(Msg::PreviewImport);
            }
            Msg::PreviewImport => {
                if let Some(Dialog::Import(form)) = &mut self.dialog {
                    form.previewed_for = form.path.clone();
                    let path = PathBuf::from(form.path.trim());
                    form.ckan_install = ckan_import::is_ckan_install(&path);
                    if form.ckan_install {
                        if form.name.is_empty() {
                            form.name = path.file_name().map_or("CKAN install".into(), |n| {
                                n.to_string_lossy().into_owned()
                            });
                        }
                        form.preview = Some(
                            ckan_import::read_registry(&path)
                                .map(|(mods, _)| {
                                    let deps = mods.iter().filter(|m| m.auto_installed).count();
                                    format!(
                                        "CKAN-managed KSP install: {} mods ({deps} dependencies), plus its saves and settings",
                                        mods.len()
                                    )
                                })
                                .map_err(|e| format!("{e:#}")),
                        );
                        return;
                    }
                    form.preview = (!form.path.trim().is_empty()).then(|| {
                        modpack::read(&PathBuf::from(form.path.trim()))
                            .map(|p| {
                                if form.name.is_empty() {
                                    form.name = p.name.clone();
                                }
                                pack_summary(&p)
                            })
                            .map_err(|e| format!("{e:#}"))
                    });
                }
            }
            Msg::Import if matches!(&self.dialog, Some(Dialog::Import(f)) if f.ckan_install) => {
                let Some(Dialog::Import(form)) = self.dialog.take() else {
                    return;
                };
                let paths = self.ctx.paths.clone();
                let dir = PathBuf::from(form.path.trim());
                let mode = if form.full_copy {
                    CopyMode::Copy
                } else {
                    CopyMode::Link
                };
                self.spawn("Importing CKAN install", move |tx| {
                    let mut p = |d, t| {
                        let _ = tx.send(TaskMsg::Copy(d, t));
                    };
                    let r = ckan_import::import(&paths, &dir, &form.name, mode, &mut p);
                    Done::CkanImported(r.map_err(|e| format!("{e:#}")))
                });
            }
            Msg::Import => {
                let Some(index) = self.index.clone() else {
                    return self
                        .error("The mod index isn't loaded yet: press Update index.".into());
                };
                if let Some(Dialog::Import(form)) = self.dialog.take() {
                    let paths = self.ctx.paths.clone();
                    let config = self.ctx.config.clone();
                    let egui = self.egui.clone();
                    let opts = modpack::ImportOptions {
                        path: PathBuf::from(form.path.trim()),
                        name: form.name.clone(),
                        base: PathBuf::from(form.base.trim()),
                        exact: form.exact,
                    };
                    self.spawn_cancellable("Importing modpack", move |tx, cancel| {
                        let r = forward_events(tx, &egui, |etx| {
                            modpack::import(&paths, &config, &index, &opts, etx, cancel)
                        });
                        Done::Imported(r.map_err(task_error))
                    });
                }
            }
            Msg::CloseDialog => self.dialog = None,
            Msg::Refresh => {
                self.steam = self.ctx.steam();
                self.steam_launch_options = launch::current_steam_launch_options(&self.steam);
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
        let result = ops::plan_manual(&view.inst, &index, &m, &self.choices);
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
        let id = inst.id.clone();
        let paths = self.ctx.paths.clone();
        let config = self.ctx.config.clone();
        let egui = self.egui.clone();
        self.spawn_cancellable("Applying changes", move |tx, cancel| {
            let r = forward_events(tx, &egui, |etx| {
                ops::apply(&paths, &config, &inst, &plan, etx, cancel)
            });
            Done::Applied(id, r.map_err(task_error))
        });
    }

    /// Writes `gui.toml` when the remembered state changed.
    fn save_prefs(&mut self) {
        if !self.persist {
            return;
        }
        let mut queues = self.queues.clone();
        if let Some(sel) = &self.selected
            && !self.queue.is_empty()
        {
            queues.insert(sel.clone(), self.queue.clone());
        }
        queues.retain(|id, q| !q.is_empty() && self.instances.iter().any(|v| &v.inst.id == id));
        let prefs = Prefs {
            selected: self.selected.clone(),
            filter: filter_name(self.filter).to_owned(),
            show_deps: self.show_deps,
            starred_only: self.starred_only,
            vim_keys_off: !self.vim_keys,
            sort: self.sort.map_or(String::new(), |(k, d)| {
                format!("{}-{}", k.name(), if d { "desc" } else { "asc" })
            }),
            queues,
        };
        if prefs != self.saved_prefs {
            if let Err(e) = prefs.save(&self.ctx.paths) {
                log::warn!("save gui.toml: {e:#}");
            }
            self.saved_prefs = prefs;
        }
    }

    fn ask_restore_history(&mut self, id: u64, before: bool) {
        let (Some(view), Some(index)) = (self.current(), self.index.clone()) else {
            return self.error("The mod index isn't loaded yet: press Update index.".into());
        };
        let Some(entry) = self.history.iter().find(|e| e.id == id) else {
            return;
        };
        let target = if before { &entry.before } else { &entry.after };
        match history::restore_plan(&self.ctx.paths, &view.inst, &index, target) {
            Ok(rp) if rp.is_empty() && rp.warnings.is_empty() => {
                self.note("The mod list already matches")
            }
            Ok(rp) => {
                let mut lines: Vec<String> = rp
                    .request
                    .remove
                    .iter()
                    .map(|r| format!("remove {r}"))
                    .collect();
                lines.extend(rp.request.install.iter().map(|(id, v)| match v {
                    Some(v) => format!("install {id} {v}"),
                    None => format!("install {id} (newest)"),
                }));
                lines.extend(
                    rp.manual
                        .iter()
                        .map(|m| format!("install manual mod {} {}", m.name, m.version)),
                );
                lines.extend(rp.warnings.iter().map(|w| format!("warning: {w}")));
                let title = format!(
                    "Put the mods back to how they were {} the change of {}?",
                    if before { "before" } else { "after" },
                    when(entry.time)
                );
                self.dialog = Some(Dialog::RestoreHistory {
                    id,
                    before,
                    title,
                    lines,
                });
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn launch(&mut self) {
        let Some(view) = self.current() else { return };
        let mut inst = view.inst.clone();
        if self.running.iter().any(|(id, _)| *id == inst.id) {
            self.note(format!("{} is already running", inst.config.name));
            return;
        }
        match backup::snapshot(&inst, "before launch", self.ctx.config.backups_keep) {
            Ok(Some(b)) => self.note(format!("Backed up {} save(s)", b.saves.len())),
            Ok(None) => {}
            Err(e) => self.note(format!("Save backup failed: {e:#}")),
        }
        if inst.config.launch.via_steam {
            let result = instance::sync_links(&mut inst)
                .and_then(|()| launch::launch_via_steam(&self.ctx.paths, &inst, &self.steam));
            match result {
                Ok(()) => {
                    self.note(format!("Asked Steam to start {}", inst.config.name));
                    inst.config.last_played = Some(orbis::paths::now_secs());
                    if let Err(e) = inst.save() {
                        log::warn!("save last played: {e:#}");
                    }
                    self.reload();
                }
                Err(e) => self.error(format!("Launch failed: {e:#}")),
            }
            return;
        }
        let result = instance::sync_links(&mut inst)
            .and_then(|()| launch::plan(&inst, &self.steam, &self.ctx.config))
            .and_then(|p| {
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
            via_steam: c.launch.via_steam,
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
            backups_keep: c.backups_keep.to_string(),
            auto_update_hours: c.auto_update_hours.to_string(),
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
        inst.config.launch.via_steam = f.via_steam;
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
        let Ok(backups_keep) = g.backups_keep.trim().parse() else {
            return self.error("Backups to keep must be a number".into());
        };
        let Ok(auto_update_hours) = g.auto_update_hours.trim().parse() else {
            return self.error("Auto-update hours must be a number".into());
        };
        let cfg = Config {
            steam_root: opt_path(&g.steam_root),
            proton: opt_path(&g.proton),
            use_steam_runtime: g.use_runtime,
            repositories: repos,
            download_workers: workers,
            backups_keep,
            auto_update_hours,
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

/// The date part of a CKAN release_date.
fn date(s: Option<&str>) -> String {
    s.map(|d| d.chars().take(10).collect()).unwrap_or_default()
}

/// A Unix time as "YYYY-MM-DD HH:MM UTC".
pub fn when(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Marks a task error as a cancellation, which is reported as a note.
const CANCELLED: &str = "\u{2063}cancelled";

/// Formats a task error, marking cancellations.
fn task_error(e: anyhow::Error) -> String {
    if orbis::cancel::is_cancelled(&e) {
        CANCELLED.to_owned()
    } else {
        format!("{e:#}")
    }
}

fn filter_from(s: &str) -> Option<Filter> {
    match s {
        "installed" => Some(Filter::Installed),
        "available" | "compatible" => Some(Filter::Available),
        "upgradable" => Some(Filter::Upgradable),
        "all" => Some(Filter::All),
        _ => None,
    }
}

fn filter_name(f: Filter) -> &'static str {
    match f {
        Filter::Installed => "installed",
        Filter::Available => "available",
        Filter::Upgradable => "upgradable",
        Filter::All => "all",
    }
}

/// Runs `work` on this thread, forwarding its progress events to the task
/// channel and waking the GUI for each.
fn forward_events<T>(
    tx: &Sender<TaskMsg>,
    egui: &egui::Context,
    work: impl FnOnce(&Sender<ops::Event>) -> Result<T>,
) -> Result<T> {
    let (etx, erx) = mpsc::channel::<ops::Event>();
    std::thread::scope(|s| {
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
        let r = work(&etx);
        drop(etx);
        r
    })
}

/// A short description of a pack for the import dialog.
fn pack_summary(p: &modpack::Pack) -> String {
    let deps = p.mods.iter().filter(|m| m.dependency).count();
    let mut parts = vec![format!(
        "{}: {} mods ({} dependencies)",
        p.name,
        p.mods.len(),
        deps
    )];
    if !p.manual.is_empty() {
        let included = p.manual.iter().filter(|m| m.included).count();
        parts.push(format!(
            "{} manual mods ({included} with files)",
            p.manual.len()
        ));
    }
    if !p.saves.is_empty() {
        parts.push(format!("saves: {}", p.saves.join(", ")));
    }
    if p.craft {
        parts.push("craft included".into());
    }
    if !p.game_version.is_any() {
        parts.push(format!("made for KSP {}", p.game_version));
    }
    parts.join(" · ")
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
        } else if orbis::install::is_packed(&path) {
            orbis::install::Source::Packed(path.clone())
        } else {
            anyhow::bail!("pick a .zip, .7z or .rar file, or a folder");
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
