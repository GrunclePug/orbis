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
#[derive(Args, Debug, Default, Clone)]
pub struct Options {
    /// Render offscreen instead of opening a window (works on lavapipe).
    #[arg(long)]
    pub headless: bool,
    /// Frames to run before the screenshot (headless).
    #[arg(long, default_value_t = 3)]
    pub frames: u32,
    /// Write a PNG of the last frame here (headless).
    #[arg(long, value_name = "PNG")]
    pub screenshot: Option<PathBuf>,
    /// Write a text dump of the interface state here (headless).
    #[arg(long, value_name = "FILE")]
    pub dump: Option<PathBuf>,
    /// Window size, WIDTHxHEIGHT in points.
    #[arg(long, default_value = "1400x860")]
    pub size: String,
    /// Select this instance on start.
    #[arg(long)]
    pub instance: Option<String>,
    /// Open this tab on start: mods, manual or settings.
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
