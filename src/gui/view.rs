//! Drawing. Reads `App` state, edits text buffers, and emits `Msg`s.

use eframe::egui::{self, Color32, RichText, Ui};

use orbis::metadata::Module;

use super::app::{App, Dialog, Filter, Msg, Row, Tab, data_paths, instance_paths};

const ACCENT: Color32 = Color32::from_rgb(110, 170, 255);
const GOOD: Color32 = Color32::from_rgb(120, 200, 120);
const WARN: Color32 = Color32::from_rgb(230, 180, 90);
const BAD: Color32 = Color32::from_rgb(235, 110, 110);
const ROW_H: f32 = 24.0;

pub fn draw(app: &mut App, ui: &mut Ui, msgs: &mut Vec<Msg>) {
    top_bar(app, ui, msgs);
    status_bar(app, ui);
    instances_panel(app, ui, msgs);
    if app.tab == Tab::Mods {
        details_panel(app, ui, msgs);
    }
    egui::CentralPanel::default_margins().show(ui, |ui| match app.tab {
        Tab::Mods => mods_tab(app, ui, msgs),
        Tab::Manual => manual_tab(app, ui, msgs),
        Tab::Settings => settings_tab(app, ui, msgs),
    });
    dialog(app, ui.ctx(), msgs);
}

fn top_bar(app: &App, ui: &mut Ui, msgs: &mut Vec<Msg>) {
    egui::Panel::top("top").show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("orbis").heading().strong().color(ACCENT));
            ui.separator();
            for (tab, label) in [
                (Tab::Mods, "Mods"),
                (Tab::Manual, "Manual mods"),
                (Tab::Settings, "Settings"),
            ] {
                if ui.selectable_label(app.tab == tab, label).clicked() {
                    msgs.push(Msg::SetTab(tab));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!app.busy(), egui::Button::new("Update index"))
                    .on_hover_text("Download the latest CKAN metadata")
                    .clicked()
                {
                    msgs.push(Msg::UpdateIndex);
                }
                ui.label(RichText::new(&app.index_status).weak());
            });
        });
        ui.add_space(2.0);
    });
}

fn status_bar(app: &App, ui: &mut Ui) {
    egui::Panel::bottom("status").show(ui, |ui| {
        ui.add_space(2.0);
        match &app.task {
            Some(task) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(&task.title).strong());
                    ui.label(&task.status);
                    if let Some(p) = task.progress {
                        ui.add(
                            egui::ProgressBar::new(p)
                                .desired_width(220.0)
                                .show_percentage(),
                        );
                    }
                });
                for (id, (bytes, total)) in &task.downloads {
                    ui.horizontal(|ui| {
                        let mib = *bytes as f32 / 1_048_576.0;
                        match total {
                            Some(t) if *t > 0 => {
                                let frac = (*bytes as f32 / *t as f32).min(1.0);
                                ui.add(egui::ProgressBar::new(frac).desired_width(220.0).text(
                                    format!("{mib:.1} / {:.1} MiB", *t as f32 / 1_048_576.0),
                                ));
                            }
                            _ => {
                                ui.label(format!("{mib:.1} MiB"));
                            }
                        }
                        ui.label(id);
                    });
                }
            }
            None => {
                ui.label(RichText::new(app.last_message()).weak());
            }
        }
        ui.add_space(2.0);
    });
}

fn instances_panel(app: &App, ui: &mut Ui, msgs: &mut Vec<Msg>) {
    egui::Panel::left("instances")
        .default_size(250.0)
        .size_range(200.0..=400.0)
        .show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Instances").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!app.busy(), egui::Button::new("+ New"))
                        .on_hover_text("Create an instance from the Steam install")
                        .clicked()
                    {
                        msgs.push(Msg::OpenNewInstance);
                    }
                });
            });
            ui.separator();
            let bottom_h = 132.0;
            egui::ScrollArea::vertical()
                .id_salt("instance-list")
                .max_height((ui.available_height() - bottom_h).max(60.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if app.instances.is_empty() {
                        ui.label(
                            RichText::new(
                                "No instances yet.\nPress + New to copy the stock game from Steam.",
                            )
                            .weak(),
                        );
                    }
                    for v in &app.instances {
                        let selected = app.selected.as_ref() == Some(&v.inst.id);
                        let running = app.running.iter().any(|(id, _)| *id == v.inst.id);
                        let frame = egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .corner_radius(6.0)
                            .fill(if selected {
                                ui.visuals().selection.bg_fill.gamma_multiply(0.5)
                            } else {
                                Color32::TRANSPARENT
                            });
                        let resp = frame
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&v.inst.config.name).strong());
                                    if running {
                                        ui.label(RichText::new("running").small().color(GOOD));
                                    }
                                });
                                ui.label(
                                    RichText::new(format!(
                                        "KSP {} · {} mods",
                                        v.inst.config.game_version.without_build(),
                                        v.reg.mods.len()
                                    ))
                                    .small()
                                    .weak(),
                                );
                            })
                            .response
                            .interact(egui::Sense::click());
                        if resp.clicked() {
                            msgs.push(Msg::SelectInstance(v.inst.id.clone()));
                        }
                    }
                });
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                ui.add_space(6.0);
                let has = app.current().is_some();
                let launch = egui::Button::new(RichText::new("Launch").strong().size(16.0))
                    .min_size(egui::vec2(ui.available_width(), 36.0))
                    .fill(Color32::from_rgb(40, 110, 60));
                if ui
                    .add_enabled(has, launch)
                    .on_hover_text("Run this instance through Proton")
                    .clicked()
                {
                    msgs.push(Msg::Launch);
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(has && !app.busy(), egui::Button::new("Clone"))
                        .clicked()
                    {
                        msgs.push(Msg::OpenClone);
                    }
                    if ui
                        .add_enabled(has && !app.busy(), egui::Button::new("Delete"))
                        .clicked()
                    {
                        msgs.push(Msg::OpenDelete);
                    }
                    if let Some(v) = app.current() {
                        ui.menu_button("Open", |ui| {
                            for (label, path) in instance_paths(v) {
                                if ui.button(label).clicked() {
                                    msgs.push(Msg::OpenFolder(path));
                                    ui.close();
                                }
                            }
                        });
                    }
                });
            });
        });
}

fn mods_tab(app: &mut App, ui: &mut Ui, msgs: &mut Vec<Msg>) {
    if app.current().is_none() {
        ui.centered_and_justified(|ui| {
            ui.label("Create or select an instance to manage its mods.");
        });
        return;
    }
    ui.horizontal(|ui| {
        for f in Filter::ALL {
            if ui.selectable_label(app.filter == f, f.label()).clicked() {
                msgs.push(Msg::SetFilter(f));
            }
        }
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut app.search)
                .hint_text("Search name, author, tag")
                .desired_width(260.0),
        );
        if !app.search.is_empty() && ui.small_button("x").clicked() {
            app.search.clear();
        }
        ui.label(RichText::new(format!("{} mods", app.rows.len())).weak());
    });
    ui.add_space(4.0);
    queue_bar(app, ui, msgs);
    ui.separator();

    let width = ui.available_width();
    let cols = columns(width);
    header(ui, &cols);
    egui::ScrollArea::vertical()
        .id_salt("mods")
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, app.rows.len(), |ui, range| {
            for i in range {
                let row = &app.rows[i];
                mod_row(app, ui, row, &cols, msgs);
            }
        });
}

fn queue_bar(app: &App, ui: &mut Ui, msgs: &mut Vec<Msg>) {
    ui.horizontal(|ui| {
        let q = &app.queue;
        if q.is_empty() {
            ui.label(RichText::new("Tick mods to install, untick to remove, then apply.").weak());
        } else {
            let mut parts = Vec::new();
            if !q.install.is_empty() {
                parts.push(format!("{} to install", q.install.len()));
            }
            if !q.upgrade.is_empty() {
                parts.push(format!("{} to upgrade", q.upgrade.len()));
            }
            if !q.remove.is_empty() {
                parts.push(format!("{} to remove", q.remove.len()));
            }
            ui.label(RichText::new(parts.join(" · ")).color(WARN));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let ready = !app.queue.is_empty() && !app.busy();
            if ui
                .add_enabled(
                    ready,
                    egui::Button::new(RichText::new("Apply changes").strong()),
                )
                .clicked()
            {
                msgs.push(Msg::ApplyQueue);
            }
            if ui
                .add_enabled(!app.queue.is_empty(), egui::Button::new("Clear"))
                .clicked()
            {
                msgs.push(Msg::ClearQueue);
            }
            let upgrades = app.rows.iter().filter(|r| r.upgradable()).count();
            if (app.filter == Filter::Upgradable || upgrades > 0)
                && ui
                    .add_enabled(app.index.is_some(), egui::Button::new("Upgrade all"))
                    .clicked()
            {
                msgs.push(Msg::QueueAllUpgrades);
            }
        });
    });
}

struct Cols {
    action: f32,
    name: f32,
    installed: f32,
    latest: f32,
    game: f32,
    downloads: f32,
}

fn columns(width: f32) -> Cols {
    let fixed = 34.0 + 110.0 + 110.0 + 110.0 + 90.0 + 8.0 * 6.0;
    Cols {
        action: 34.0,
        name: (width - fixed).max(160.0),
        installed: 110.0,
        latest: 110.0,
        game: 110.0,
        downloads: 90.0,
    }
}

fn header(ui: &mut Ui, c: &Cols) {
    ui.horizontal(|ui| {
        let h = |ui: &mut Ui, w: f32, t: &str| {
            cell(ui, w, egui::Label::new(RichText::new(t).small().strong()));
        };
        h(ui, c.action, "");
        h(ui, c.name, "Name");
        h(ui, c.installed, "Installed");
        h(ui, c.latest, "Latest");
        h(ui, c.game, "KSP");
        h(ui, c.downloads, "Downloads");
    });
}

fn mod_row(app: &App, ui: &mut Ui, row: &Row, c: &Cols, msgs: &mut Vec<Msg>) {
    let q = &app.queue;
    let selected = app.selected_mod.as_ref() == Some(&row.id);
    let outer = ui.horizontal(|ui| {
        ui.set_height(ROW_H);
        // Action checkbox: ticked = installed (or queued for install).
        cell_ui(ui, c.action, |ui| {
            if row.dlc {
                ui.label(RichText::new("DLC").small().weak());
                return;
            }
            let installed = row.installed.is_some();
            let mut ticked = if installed {
                !q.remove.contains(&row.id)
            } else {
                q.install.contains(&row.id)
            };
            let enabled = installed || row.latest.is_some();
            if ui
                .add_enabled(enabled, egui::Checkbox::without_text(&mut ticked))
                .changed()
            {
                msgs.push(if installed {
                    Msg::ToggleRemove(row.id.clone())
                } else {
                    Msg::ToggleInstall(row.id.clone())
                });
            }
        });
        let mut name = RichText::new(&row.name);
        if q.remove.contains(&row.id) {
            name = name.strikethrough().color(BAD);
        } else if q.install.contains(&row.id) {
            name = name.color(GOOD);
        } else if row.installed.is_none() && row.latest.is_none() {
            name = name.weak();
        }
        if row.manual {
            name = name.color(ACCENT);
        }
        let name_label = egui::Label::new(name)
            .truncate()
            .sense(egui::Sense::click());
        let name_resp = cell(ui, c.name, name_label).on_hover_text(&row.summary);
        let tag = if row.manual {
            " manual"
        } else if row.auto {
            " auto"
        } else {
            ""
        };
        let inst = row
            .installed
            .as_ref()
            .map_or(String::new(), |v| format!("{v}{tag}"));
        cell(
            ui,
            c.installed,
            egui::Label::new(RichText::new(inst).small()).truncate(),
        );
        cell_ui(ui, c.latest, |ui| {
            let latest = row.latest.as_ref().map_or("—".into(), ToString::to_string);
            if row.upgradable() {
                let mut up = q.upgrade.contains(&row.id);
                if ui
                    .checkbox(
                        &mut up,
                        RichText::new(format!("upgrade {latest}"))
                            .small()
                            .color(WARN),
                    )
                    .on_hover_text("Queue an upgrade")
                    .changed()
                {
                    msgs.push(Msg::ToggleUpgrade(row.id.clone()));
                }
            } else {
                ui.add(egui::Label::new(RichText::new(latest).small().weak()).truncate());
            }
        });
        cell(
            ui,
            c.game,
            egui::Label::new(RichText::new(&row.game).small().weak()).truncate(),
        );
        let dl = if row.downloads > 0 {
            human(row.downloads)
        } else {
            String::new()
        };
        cell(
            ui,
            c.downloads,
            egui::Label::new(RichText::new(dl).small().weak()),
        );
        name_resp
    });
    let resp = outer.inner;
    if selected {
        ui.painter().rect_filled(
            outer.response.rect.expand2(egui::vec2(4.0, 0.0)),
            3.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.25),
        );
    }
    if resp.clicked() {
        msgs.push(Msg::SelectMod(row.id.clone()));
    }
}

/// A fixed-width, left-aligned table cell holding one widget.
fn cell(ui: &mut Ui, width: f32, widget: impl egui::Widget) -> egui::Response {
    cell_ui(ui, width, |ui| ui.add(widget))
}

/// A fixed-width, left-aligned table cell.
fn cell_ui<R>(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        egui::vec2(width, ROW_H),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_width(width);
            ui.set_height(ROW_H);
            add(ui)
        },
    )
    .inner
}

fn human(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn details_panel(app: &App, ui: &mut Ui, msgs: &mut Vec<Msg>) {
    egui::Panel::right("details")
        .default_size(360.0)
        .size_range(260.0..=600.0)
        .show(ui, |ui| {
            let Some(m) = app.selected_module() else {
                ui.add_space(12.0);
                ui.label(RichText::new("Select a mod to see its details.").weak());
                return;
            };
            egui::ScrollArea::vertical()
                .id_salt("details")
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new(m.display_name()).heading().strong());
                    ui.label(
                        RichText::new(format!("{} · {}", m.identifier, m.version))
                            .small()
                            .weak(),
                    );
                    ui.add_space(4.0);
                    actions(app, ui, &m, msgs);
                    ui.add_space(6.0);
                    ui.label(&m.summary);
                    ui.separator();
                    egui::Grid::new("facts")
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            let fact = |ui: &mut Ui, k: &str, v: String| {
                                ui.label(RichText::new(k).weak());
                                ui.add(egui::Label::new(v).wrap());
                                ui.end_row();
                            };
                            fact(ui, "Authors", m.author.join(", "));
                            fact(ui, "License", m.license.join(", "));
                            fact(ui, "KSP", m.game_range_label());
                            if let Some(index) = &app.index {
                                fact(ui, "Downloads", human(index.download_count(&m.identifier)));
                            }
                            if let Some(size) = m.download_size {
                                fact(ui, "Size", format!("{:.1} MiB", size as f64 / 1_048_576.0));
                            }
                            if let Some(d) = &m.release_date {
                                fact(ui, "Released", d.chars().take(10).collect());
                            }
                        });
                    let links: Vec<(&str, &str)> = [
                        "homepage",
                        "repository",
                        "spacedock",
                        "bugtracker",
                        "manual",
                    ]
                    .iter()
                    .filter_map(|k| m.resource(k).map(|v| (*k, v)))
                    .collect();
                    if !links.is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            for (k, url) in links {
                                if ui.link(k).on_hover_text(url).clicked() {
                                    msgs.push(Msg::OpenUrl(url.to_owned()));
                                }
                            }
                        });
                    }
                    relationships(
                        ui,
                        "Depends on",
                        &m.depends
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    );
                    relationships(
                        ui,
                        "Recommends",
                        &m.recommends
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    );
                    relationships(
                        ui,
                        "Suggests",
                        &m.suggests
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    );
                    relationships(
                        ui,
                        "Conflicts with",
                        &m.conflicts
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    );
                    relationships(ui, "Provides", &m.provides);
                    if let Some(d) = &m.description {
                        ui.separator();
                        ui.add(egui::Label::new(RichText::new(d).small()).wrap());
                    }
                    if let Some(index) = &app.index {
                        let versions = index.versions(&m.identifier);
                        if versions.len() > 1 {
                            ui.separator();
                            egui::CollapsingHeader::new(format!("Releases ({})", versions.len()))
                                .id_salt("releases")
                                .show(ui, |ui| {
                                    for v in versions.iter().take(30) {
                                        ui.label(
                                            RichText::new(format!(
                                                "{}   KSP {}",
                                                v.version,
                                                v.game_range_label()
                                            ))
                                            .small(),
                                        );
                                    }
                                });
                        }
                    }
                });
        });
}

fn actions(app: &App, ui: &mut Ui, m: &Module, msgs: &mut Vec<Msg>) {
    let Some(view) = app.current() else { return };
    let installed = view.reg.mods.get(&m.identifier);
    let row = app.rows.iter().find(|r| r.id == m.identifier);
    ui.horizontal(|ui| match installed {
        Some(i) => {
            ui.label(RichText::new(format!("Installed {}", i.module.version)).color(GOOD));
            let queued = app.queue.remove.contains(&m.identifier);
            if ui.button(if queued { "Keep" } else { "Remove" }).clicked() {
                msgs.push(Msg::ToggleRemove(m.identifier.clone()));
            }
            if row.is_some_and(Row::upgradable) {
                let queued = app.queue.upgrade.contains(&m.identifier);
                if ui
                    .button(if queued { "Don't upgrade" } else { "Upgrade" })
                    .clicked()
                {
                    msgs.push(Msg::ToggleUpgrade(m.identifier.clone()));
                }
            }
        }
        None => {
            let compatible = app.index.as_ref().is_some_and(|i| {
                i.latest_compatible(
                    &m.identifier,
                    &view.inst.compat_versions(),
                    view.inst.config.allow_testing,
                )
                .is_some()
            });
            if !compatible {
                ui.label(RichText::new("Not compatible with this instance").color(WARN));
            }
            let queued = app.queue.install.contains(&m.identifier);
            if ui
                .add_enabled(
                    compatible,
                    egui::Button::new(if queued { "Unqueue" } else { "Install" }),
                )
                .clicked()
            {
                msgs.push(Msg::ToggleInstall(m.identifier.clone()));
            }
        }
    });
}

fn relationships(ui: &mut Ui, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.label(RichText::new(title).weak());
    ui.horizontal_wrapped(|ui| {
        for i in items {
            ui.label(RichText::new(i).small().monospace());
        }
    });
}

fn manual_tab(app: &App, ui: &mut Ui, msgs: &mut Vec<Msg>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Manual mod library").strong());
        ui.label(
            RichText::new("Mods from outside CKAN (Patreon builds, betas, patches), stored once for every instance.")
                .weak(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add_enabled(!app.busy(), egui::Button::new("+ Add mod...")).clicked() {
                msgs.push(Msg::OpenAddManual(None));
            }
        });
    });
    ui.label(
        RichText::new("Tip: drop a .zip or folder onto the window to add it.")
            .small()
            .weak(),
    );
    ui.separator();
    if app.manual.is_empty() {
        ui.add_space(20.0);
        ui.label(RichText::new("The library is empty.").weak());
        return;
    }
    let view = app.current();
    egui::ScrollArea::vertical()
        .id_salt("manual")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for m in &app.manual {
                let installed = view.and_then(|v| v.reg.mods.get(&m.identifier()));
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&m.name).strong());
                        ui.label(RichText::new(&m.version).weak());
                        if let Some(i) = installed {
                            ui.label(
                                RichText::new(format!("installed {}", i.module.version))
                                    .small()
                                    .color(GOOD),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("Delete")
                                .on_hover_text("Remove from the library")
                                .clicked()
                            {
                                msgs.push(Msg::AskDeleteManual(m.id.clone()));
                            }
                            if view.is_some() {
                                if installed.is_some() {
                                    if ui
                                        .add_enabled(
                                            !app.busy(),
                                            egui::Button::new("Remove from instance"),
                                        )
                                        .clicked()
                                    {
                                        msgs.push(Msg::RemoveManual(m.id.clone()));
                                    }
                                } else if ui
                                    .add_enabled(
                                        !app.busy(),
                                        egui::Button::new("Install into instance"),
                                    )
                                    .clicked()
                                {
                                    msgs.push(Msg::InstallManual(m.id.clone()));
                                }
                            }
                        });
                    });
                    let mut facts = Vec::new();
                    if !m.provides.is_empty() {
                        facts.push(format!("replaces {}", m.provides.join(", ")));
                    }
                    if !m.depends.is_empty() {
                        facts.push(format!("needs {}", m.depends.join(", ")));
                    }
                    if !m.conflicts.is_empty() {
                        facts.push(format!("conflicts with {}", m.conflicts.join(", ")));
                    }
                    facts.push(
                        m.install
                            .iter()
                            .map(orbis::install::describe)
                            .collect::<Vec<_>>()
                            .join("; "),
                    );
                    ui.label(RichText::new(facts.join(" · ")).small().weak());
                    if !m.source_url.is_empty()
                        && ui.link(RichText::new(&m.source_url).small()).clicked()
                    {
                        msgs.push(Msg::OpenUrl(m.source_url.clone()));
                    }
                });
            }
        });
}

fn settings_tab(app: &mut App, ui: &mut Ui, msgs: &mut Vec<Msg>) {
    app.sync_settings_form();
    app.sync_global_form();
    egui::ScrollArea::vertical()
        .id_salt("settings")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if app.current().is_some() {
                ui.label(RichText::new("Instance").heading());
                let f = &mut app.settings;
                egui::Grid::new("instance-settings")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add(egui::TextEdit::singleline(&mut f.name).desired_width(320.0));
                        ui.end_row();
                        ui.label("Also compatible with");
                        ui.add(
                            egui::TextEdit::singleline(&mut f.compat)
                                .hint_text("1.12, 1.11, 1.10")
                                .desired_width(320.0),
                        )
                        .on_hover_text("Mods for these KSP versions are offered too");
                        ui.end_row();
                        ui.label("Testing releases");
                        ui.checkbox(&mut f.allow_testing, "Offer releases marked testing");
                        ui.end_row();
                        ui.label("Wine prefix");
                        ui.checkbox(
                            &mut f.prefix_isolated,
                            "Use a separate prefix for this instance",
                        )
                        .on_hover_text(
                            "Off: share Steam's KSP prefix (KSP keeps its data in the game folder)",
                        );
                        ui.end_row();
                        ui.label("Launch arguments");
                        ui.add(
                            egui::TextEdit::singleline(&mut f.args)
                                .hint_text("-popupwindow")
                                .desired_width(320.0),
                        );
                        ui.end_row();
                        ui.label("Environment");
                        ui.add(
                            egui::TextEdit::multiline(&mut f.env)
                                .hint_text("DXVK_HUD=fps\nPROTON_ENABLE_WAYLAND=1")
                                .desired_rows(3)
                                .desired_width(320.0),
                        );
                        ui.end_row();
                        ui.label("Notes");
                        ui.add(
                            egui::TextEdit::multiline(&mut f.notes)
                                .desired_rows(2)
                                .desired_width(320.0),
                        );
                        ui.end_row();
                    });
                if ui.button("Save instance settings").clicked() {
                    msgs.push(Msg::SaveInstanceSettings);
                }
                if let Some(v) = app.current() {
                    ui.label(
                        RichText::new(format!("Directory: {}", v.inst.dir.display()))
                            .small()
                            .weak(),
                    );
                    let layout = match &v.inst.config.base {
                        Some(b) => format!("Linked to: {}", b.display()),
                        None => "Full copy".to_owned(),
                    };
                    ui.label(RichText::new(layout).small().weak());
                    let dlc: Vec<String> = v.dlc.iter().map(|d| d.identifier.clone()).collect();
                    if !dlc.is_empty() {
                        ui.label(
                            RichText::new(format!("DLC: {}", dlc.join(", ")))
                                .small()
                                .weak(),
                        );
                    }
                }
                ui.add_space(12.0);
                ui.separator();
            }

            ui.label(RichText::new("Steam and Proton").heading());
            egui::Grid::new("steam")
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    for (k, v) in app.steam.report() {
                        ui.label(RichText::new(k).weak());
                        let color = if v == "not found" || v == "not set" {
                            WARN
                        } else {
                            ui.visuals().text_color()
                        };
                        ui.label(RichText::new(v).color(color));
                        ui.end_row();
                    }
                });
            ui.add_space(8.0);
            let g = &mut app.global;
            egui::Grid::new("global-settings")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Steam root");
                    ui.add(
                        egui::TextEdit::singleline(&mut g.steam_root)
                            .hint_text("detected")
                            .desired_width(420.0),
                    );
                    ui.end_row();
                    ui.label("Proton directory");
                    ui.add(
                        egui::TextEdit::singleline(&mut g.proton)
                            .hint_text("the Proton Steam uses for KSP")
                            .desired_width(420.0),
                    );
                    ui.end_row();
                    ui.label("Steam Runtime");
                    ui.checkbox(
                        &mut g.use_runtime,
                        "Run Proton inside the Steam Linux Runtime when it asks to",
                    );
                    ui.end_row();
                    ui.label("Parallel downloads");
                    ui.add(egui::TextEdit::singleline(&mut g.workers).desired_width(60.0));
                    ui.end_row();
                    ui.label("Repositories");
                    ui.add(
                        egui::TextEdit::multiline(&mut g.repos)
                            .hint_text("name url")
                            .desired_rows(2)
                            .desired_width(420.0),
                    );
                    ui.end_row();
                });
            ui.horizontal(|ui| {
                if ui.button("Save global settings").clicked() {
                    msgs.push(Msg::SaveGlobalSettings);
                }
                if ui.button("Re-detect").clicked() {
                    msgs.push(Msg::Refresh);
                }
            });
            ui.add_space(8.0);
            for (label, path) in data_paths(&app.ctx.paths) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(label).weak());
                    if ui.link(path.display().to_string()).clicked() {
                        msgs.push(Msg::OpenFolder(path));
                    }
                });
            }
            ui.label(
                RichText::new(format!("orbis {}", env!("CARGO_PKG_VERSION")))
                    .small()
                    .weak(),
            );
        });
}

fn dialog(app: &mut App, ctx: &egui::Context, msgs: &mut Vec<Msg>) {
    let busy = app.busy();
    let Some(d) = &mut app.dialog else { return };
    let resp = egui::Modal::new(egui::Id::unique("orbis-dialog")).show(ctx, |ui| {
        ui.set_max_width(560.0);
        match d {
            Dialog::NewInstance {
                name,
                source,
                with_saves,
                full_copy,
                keep_mods,
            } => {
                ui.heading("New instance");
                ui.label("The instance keeps its own mods, saves and settings, and links the stock game from the install below. That install is never changed, so launching it from Steam stays vanilla.");
                ui.add_space(6.0);
                egui::Grid::new("new").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(name).desired_width(360.0));
                    ui.end_row();
                    ui.label("Game install");
                    ui.add(egui::TextEdit::singleline(source).desired_width(360.0));
                    ui.end_row();
                });
                ui.checkbox(with_saves, "Copy the install's saves");
                ui.checkbox(full_copy, "Full copy instead (keeps this game version when the install updates)");
                if *full_copy {
                    ui.checkbox(keep_mods, "Also copy non-stock GameData (orbis won't manage those files)");
                }
                ui.add_space(8.0);
                buttons(ui, msgs, "Create", !busy && !name.trim().is_empty(), Msg::CreateInstance);
            }
            Dialog::CloneInstance { name, with_saves, .. } => {
                ui.heading("Clone instance");
                ui.add(egui::TextEdit::singleline(name).desired_width(360.0));
                ui.checkbox(with_saves, "Copy saves");
                ui.add_space(8.0);
                buttons(ui, msgs, "Clone", !busy && !name.trim().is_empty(), Msg::CloneInstance);
            }
            Dialog::DeleteInstance { name, .. } => {
                ui.heading("Delete instance");
                ui.label(format!("Delete {name}, including its saves and mods? This can't be undone."));
                ui.add_space(8.0);
                buttons(ui, msgs, "Delete", true, Msg::DeleteInstance);
            }
            Dialog::DeleteManual(m) => {
                ui.heading("Delete manual mod");
                ui.label(format!(
                    "Remove {} from the library? Instances that have it installed keep their files.",
                    m.name
                ));
                ui.add_space(8.0);
                buttons(ui, msgs, "Delete", true, Msg::DeleteManual);
            }
            Dialog::Confirm { plan, picks, .. } => {
                ui.heading("Apply changes");
                egui::ScrollArea::vertical().max_height(300.0).id_salt("plan").show(ui, |ui| {
                    for c in &plan.changes {
                        let (icon, color) = match c {
                            orbis::resolver::Change::Install { .. } => ("+", GOOD),
                            orbis::resolver::Change::Upgrade { .. } => ("^", WARN),
                            orbis::resolver::Change::Remove { .. } => ("-", BAD),
                        };
                        ui.label(RichText::new(format!("{icon} {c}")).color(color));
                    }
                });
                if !plan.suggestions.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("Also worth installing").strong());
                    egui::ScrollArea::vertical().max_height(180.0).id_salt("suggest").show(ui, |ui| {
                        for (s, pick) in plan.suggestions.iter().zip(picks.iter_mut()) {
                            let kind = if s.recommended { "recommends" } else { "suggests" };
                            let first = s.candidates.first().map_or("", String::as_str);
                            let more = if s.candidates.len() > 1 {
                                format!(" (or {} others)", s.candidates.len() - 1)
                            } else {
                                String::new()
                            };
                            ui.checkbox(pick, format!("{first}{more}  — {} {kind} it", s.by));
                        }
                    });
                }
                ui.add_space(8.0);
                buttons(ui, msgs, "Apply", !busy, Msg::ConfirmApply);
            }
            Dialog::Choice {
                by,
                relationship,
                candidates,
                pick,
                ..
            } => {
                ui.heading("Choose a mod");
                ui.label(format!("{by} needs {relationship}. Several mods provide it:"));
                egui::ScrollArea::vertical().max_height(300.0).id_salt("choice").show(ui, |ui| {
                    for (i, c) in candidates.iter().enumerate() {
                        ui.radio_value(pick, i, c);
                    }
                });
                ui.add_space(8.0);
                buttons(ui, msgs, "Use this", true, Msg::ChoicePicked);
            }
            Dialog::AddManual(form) => {
                ui.heading("Add manual mod");
                ui.label("A .zip or a folder. If it has a GameData folder, its contents are installed; otherwise each top-level folder goes into GameData.");
                ui.add_space(6.0);
                egui::Grid::new("add-manual").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("File or folder");
                    ui.add(egui::TextEdit::singleline(&mut form.path).hint_text("/path/to/mod.zip").desired_width(380.0));
                    ui.end_row();
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(380.0));
                    ui.end_row();
                    ui.label("Version");
                    ui.add(egui::TextEdit::singleline(&mut form.version).desired_width(120.0));
                    ui.end_row();
                    ui.label("Replaces");
                    ui.add(
                        egui::TextEdit::singleline(&mut form.provides)
                            .hint_text("CKAN ids, e.g. EnvironmentalVisualEnhancements, Scatterer")
                            .desired_width(380.0),
                    )
                    .on_hover_text("Installed CKAN copies are removed, and mods that need them are satisfied");
                    ui.end_row();
                    ui.label("Needs");
                    ui.add(
                        egui::TextEdit::singleline(&mut form.depends)
                            .hint_text("CKAN ids, e.g. ModuleManager")
                            .desired_width(380.0),
                    );
                    ui.end_row();
                    ui.label("Conflicts with");
                    ui.add(egui::TextEdit::singleline(&mut form.conflicts).desired_width(380.0));
                    ui.end_row();
                    ui.label("Only folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut form.find)
                            .hint_text("optional: install just this folder")
                            .desired_width(380.0),
                    );
                    ui.end_row();
                    ui.label("Source URL");
                    ui.add(egui::TextEdit::singleline(&mut form.url).desired_width(380.0));
                    ui.end_row();
                    ui.label("Notes");
                    ui.add(egui::TextEdit::singleline(&mut form.notes).desired_width(380.0));
                    ui.end_row();
                });
                if form.previewed_for != format!("{}|{}", form.path, form.find) {
                    msgs.push(Msg::PreviewManual);
                }
                match &form.preview {
                    Some(Ok(lines)) => {
                        for l in lines {
                            ui.label(RichText::new(l).small().monospace().color(GOOD));
                        }
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).small().color(BAD));
                    }
                    None => {}
                }
                ui.add_space(8.0);
                let ok = !busy && matches!(form.preview, Some(Ok(_))) && !form.name.trim().is_empty();
                buttons(ui, msgs, "Add to library", ok, Msg::AddManual);
            }
            Dialog::Error(e) => {
                ui.heading(RichText::new("Something went wrong").color(BAD));
                egui::ScrollArea::vertical().max_height(300.0).id_salt("err").show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(e.as_str()).monospace().small()).wrap());
                });
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    msgs.push(Msg::CloseDialog);
                }
            }
        }
    });
    if resp.should_close() {
        msgs.push(Msg::CloseDialog);
    }
}

fn buttons(ui: &mut Ui, msgs: &mut Vec<Msg>, ok: &str, enabled: bool, msg: Msg) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new(RichText::new(ok).strong()))
            .clicked()
        {
            msgs.push(msg);
        }
        if ui.button("Cancel").clicked() {
            msgs.push(Msg::CloseDialog);
        }
    });
}
