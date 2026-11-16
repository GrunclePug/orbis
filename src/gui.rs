//! Graphical interface (egui through eframe, wgpu backend).

mod app;
mod headless;
mod view;

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;

use crate::cli::Context;

pub use app::App;

/// GUI options, including the headless screenshot mode used to verify the
/// interface without a window.
#[derive(Args, Debug, Clone)]
pub struct Options {
    /// Render offscreen instead of opening a window (works on lavapipe).
    #[arg(long)]
    pub headless: bool,
    /// Frames to run before the screenshot (headless).
    #[arg(long, default_value_t = DEFAULT_FRAMES)]
    pub frames: u32,
    /// Write a PNG of the last frame here (headless).
    #[arg(long, value_name = "PNG")]
    pub screenshot: Option<PathBuf>,
    /// Write a text dump of the interface state here (headless).
    #[arg(long, value_name = "FILE")]
    pub dump: Option<PathBuf>,
    /// Window size, WIDTHxHEIGHT in points.
    #[arg(long, default_value = DEFAULT_SIZE)]
    pub size: String,
    /// Select this instance on start.
    #[arg(long)]
    pub instance: Option<String>,
    /// Open this tab on start: mods, manual, flags, history, logs or settings.
    #[arg(long)]
    pub tab: Option<String>,
    /// Start with this search text.
    #[arg(long)]
    pub search: Option<String>,
    /// Select this mod on start.
    #[arg(long, value_name = "IDENTIFIER")]
    pub select: Option<String>,
    /// Start on this filter: installed, available, upgradable or all.
    #[arg(long)]
    pub filter: Option<String>,
    /// Headless: type these characters, one per frame, before the
    /// screenshot (to check keyboard handling).
    #[arg(long)]
    pub keys: Option<String>,
}

const DEFAULT_SIZE: &str = "1400x860";
const DEFAULT_FRAMES: u32 = 3;

/// The same defaults clap applies, for `orbis` run with no command.
impl Default for Options {
    fn default() -> Self {
        Self {
            headless: false,
            frames: DEFAULT_FRAMES,
            screenshot: None,
            dump: None,
            size: DEFAULT_SIZE.into(),
            instance: None,
            tab: None,
            search: None,
            select: None,
            filter: None,
            keys: None,
        }
    }
}

impl Options {
    fn size(&self) -> Result<[f32; 2]> {
        let (w, h) = self
            .size
            .split_once('x')
            .ok_or_else(|| anyhow!("--size must look like 1400x860"))?;
        let w: f32 = w
            .trim()
            .parse()
            .map_err(|_| anyhow!("bad width in --size"))?;
        let h: f32 = h
            .trim()
            .parse()
            .map_err(|_| anyhow!("bad height in --size"))?;
        if !(320.0..=8192.0).contains(&w) || !(240.0..=8192.0).contains(&h) {
            return Err(anyhow!("--size out of range"));
        }
        Ok([w, h])
    }
}

/// Runs the GUI in a window, or headless when asked.
pub fn run(ctx: Context, opts: Options) -> Result<()> {
    let size = opts.size()?;
    if opts.headless {
        return headless::run(ctx, &opts, size);
    }
    let native = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("orbis")
            .with_app_id("orbis")
            .with_inner_size(size)
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "orbis",
        native,
        Box::new(move |cc| {
            let mut app = App::new(ctx, &opts, cc.egui_ctx.clone());
            app.start_index_load();
            Ok(Box::new(Window(app)))
        }),
    )
    .map_err(|e| anyhow!("GUI failed: {e}"))
}

struct Window(App);

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        self.0.frame(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrap {
        #[command(flatten)]
        opts: Options,
    }

    #[test]
    fn queue_prune_drops_changes_that_already_happened() {
        let module = |id: &str| {
            serde_json::json!({
                "source": "ckan", "auto_installed": false, "installed_at": 0,
                "files": [], "dirs": [],
                "module": { "identifier": id, "version": "1.0" }
            })
        };
        let reg: orbis::instance::Registry = serde_json::from_value(serde_json::json!({
            "mods": { "Kept": module("Kept"), "Installed": module("Installed") }
        }))
        .expect("registry");
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        let mut q = app::Queue {
            install: set(&["Installed", "New"]),
            remove: set(&["Kept", "Gone"]),
            upgrade: set(&["Gone", "Kept"]),
        };
        q.prune(&reg);
        assert_eq!(q.install, set(&["New"]));
        assert_eq!(q.remove, set(&["Kept"]));
        assert_eq!(q.upgrade, set(&["Kept"]));
    }

    #[test]
    fn default_matches_clap_defaults() {
        let parsed = Wrap::parse_from(["orbis"]).opts;
        let default = Options::default();
        assert_eq!(default.size, parsed.size);
        assert_eq!(default.frames, parsed.frames);
        assert_eq!(
            default.size().expect("default size parses"),
            [1400.0, 860.0]
        );
    }
}
