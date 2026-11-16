//! The look of the interface: colours, fonts, sizes and a few shared
//! building blocks (pills, cards, section titles, buttons), so every tab
//! speaks the same visual language.
//!
//! Colours carry meaning everywhere they appear:
//! green = installed, fits, good to go; amber = an update or something that
//! needs a look; red = removal, missing, danger; blue = the accent and what's
//! selected or queued; violet = things orbis manages for you (dependencies,
//! manual mods); teal = stock content (DLC) and the SPH.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, RichText,
    Stroke, TextStyle, Ui,
};

// ---- palette ------------------------------------------------------------------

/// The darkest layer: the area behind the content.
pub const BG: Color32 = Color32::from_rgb(14, 17, 22);
/// Side panels and bars.
pub const PANEL: Color32 = Color32::from_rgb(19, 23, 29);
/// Cards, table headers and inputs.
pub const SURFACE: Color32 = Color32::from_rgb(25, 30, 38);
/// Hovered surfaces.
pub const RAISED: Color32 = Color32::from_rgb(33, 40, 50);
/// Hairlines between areas.
pub const BORDER: Color32 = Color32::from_rgb(40, 47, 58);

pub const TEXT: Color32 = Color32::from_rgb(226, 231, 238);
pub const MUTED: Color32 = Color32::from_rgb(152, 162, 176);
/// Quiet text that still reads: about 5.4:1 on `BG`, 4.8:1 on `SURFACE`.
pub const FAINT: Color32 = Color32::from_rgb(128, 138, 152);

pub const ACCENT: Color32 = Color32::from_rgb(92, 157, 255);
pub const GOOD: Color32 = Color32::from_rgb(70, 190, 125);
pub const WARN: Color32 = Color32::from_rgb(232, 170, 72);
pub const BAD: Color32 = Color32::from_rgb(240, 100, 96);
/// Errors that aren't exceptions: between amber and red.
pub const ERROR: Color32 = Color32::from_rgb(240, 140, 100);
pub const VIOLET: Color32 = Color32::from_rgb(168, 140, 250);
pub const TEAL: Color32 = Color32::from_rgb(56, 200, 190);

/// A colour at low strength, for backgrounds behind text of that colour.
pub fn tint(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

// ---- fonts and sizes ----------------------------------------------------------

const SEMIBOLD: &str = "semibold";

/// Installs the fonts, sizes, colours and spacing.
pub fn apply(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "noto".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/NotoSans-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "noto-semibold".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/NotoSans-SemiBold.ttf"
        ))),
    );
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut regular = vec!["noto".to_owned()];
    regular.extend(fallback.iter().cloned());
    let mut bold = vec!["noto-semibold".to_owned()];
    bold.extend(fallback);
    fonts.families.insert(FontFamily::Proportional, regular);
    fonts
        .families
        .insert(FontFamily::Name(SEMIBOLD.into()), bold);
    ctx.set_fonts(fonts);

    ctx.global_style_mut(|s| {
        s.text_styles = [
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
            (
                TextStyle::Heading,
                FontId::new(19.0, FontFamily::Name(SEMIBOLD.into())),
            ),
        ]
        .into();

        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.window_margin = Margin::same(16);
        s.spacing.menu_margin = Margin::same(6);

        let v = &mut s.visuals;
        v.dark_mode = true;
        v.override_text_color = None;
        v.panel_fill = PANEL;
        v.window_fill = SURFACE;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.extreme_bg_color = BG;
        v.faint_bg_color = tint(Color32::WHITE, 6);
        v.code_bg_color = SURFACE;
        v.text_edit_bg_color = Some(BG);
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = WARN;
        v.error_fg_color = BAD;
        v.weak_text_color = Some(MUTED);
        v.selection.bg_fill = tint(ACCENT, 70);
        v.selection.stroke = Stroke::new(1.0, ACCENT);

        let radius = CornerRadius::same(6);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.weak_bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = radius;
        for (state, fill, stroke) in [
            (&mut w.inactive, SURFACE, BORDER),
            (&mut w.hovered, RAISED, tint(ACCENT, 90)),
            (&mut w.active, RAISED, ACCENT),
            (&mut w.open, RAISED, BORDER),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = Stroke::new(1.0, stroke);
            state.fg_stroke = Stroke::new(1.0, TEXT);
            state.corner_radius = radius;
            state.expansion = 0.0;
        }
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    });
}

/// Text in the semibold face, for names and titles.
pub fn semibold(text: impl Into<String>) -> RichText {
    RichText::new(text).family(FontFamily::Name(SEMIBOLD.into()))
}

// ---- building blocks ----------------------------------------------------------

/// A small rounded label in a colour, on a faint wash of the same colour.
/// It is one widget, so wrapping rows move it to the next line whole.
pub fn pill(ui: &mut Ui, text: impl Into<String>, color: Color32) -> egui::Response {
    let font = TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.into(), font, color);
    let pad = egui::vec2(7.0, 2.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(9), tint(color, 34));
        ui.painter().galley(rect.min + pad, galley, color);
    }
    resp
}

/// How wide `pill` draws this text.
pub fn pill_width(ui: &Ui, text: &str) -> f32 {
    let font = TextStyle::Small.resolve(ui.style());
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, TEXT)
        .size()
        .x
        + 14.0
}

/// The semibold face at a size, for painting text directly.
pub fn semibold_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

/// Each mod category's colour, shown only as a dot and a bar beside rows.
/// The hues are muted and kept off the status colours (green installed,
/// amber updates, red removal), so a category never reads as a state. The
/// user's own categories get one from a spare set, always the same for the
/// same name.
pub fn category_color(category: &str) -> Color32 {
    let rgb = Color32::from_rgb;
    match category {
        "Parts" => rgb(110, 165, 230),
        "Visuals" => rgb(225, 125, 185),
        "Planets" => rgb(85, 195, 215),
        "Communications" => rgb(170, 145, 235),
        "Career & science" => rgb(220, 205, 110),
        "Tools & info" => rgb(135, 150, 245),
        "KSC & launch sites" => rgb(205, 125, 220),
        "Crew & IVA" => rgb(150, 205, 230),
        "Resources" => rgb(195, 170, 140),
        "Physics" => rgb(240, 160, 215),
        "Sound" => rgb(115, 205, 200),
        "Patches & configs" => rgb(150, 160, 178),
        "Libraries" => rgb(150, 140, 190),
        "Other" => rgb(120, 128, 140),
        custom => {
            const SPARE: [Color32; 6] = [
                Color32::from_rgb(100, 180, 245),
                Color32::from_rgb(200, 150, 240),
                Color32::from_rgb(240, 140, 200),
                Color32::from_rgb(100, 210, 225),
                Color32::from_rgb(160, 170, 250),
                Color32::from_rgb(225, 215, 130),
            ];
            let sum = custom.to_lowercase().bytes().fold(0usize, |a, b| {
                a.wrapping_mul(31).wrapping_add(usize::from(b))
            });
            SPARE[sum % SPARE.len()]
        }
    }
}

/// A section title: small, spaced capitals in a quiet colour.
pub fn section(ui: &mut Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(semibold(title.to_uppercase()).small().color(FAINT));
    ui.add_space(2.0);
}

/// The frame of a card: a raised surface with a hairline border.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(10))
}

/// The main action of a view, filled with `color`.
pub fn primary(text: &str, color: Color32) -> egui::Button<'static> {
    egui::Button::new(semibold(text).color(Color32::from_rgb(10, 14, 20)))
        .fill(color)
        .stroke(Stroke::NONE)
}

/// A destructive action: quiet until it matters.
pub fn danger(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(BAD))
}

/// A tab in a row of tabs: plain text, underlined in the accent when active.
pub fn tab(ui: &mut Ui, active: bool, label: &str) -> egui::Response {
    let text = if active {
        semibold(label).color(TEXT)
    } else {
        RichText::new(label).color(MUTED)
    };
    let resp = ui.add(egui::Button::new(text).frame(false));
    if active {
        let r = resp.rect;
        ui.painter().line_segment(
            [
                egui::pos2(r.left() + 4.0, r.bottom() + 3.0),
                egui::pos2(r.right() - 4.0, r.bottom() + 3.0),
            ],
            Stroke::new(2.0, ACCENT),
        );
    } else if resp.hovered() {
        ui.painter().line_segment(
            [
                egui::pos2(resp.rect.left() + 4.0, resp.rect.bottom() + 3.0),
                egui::pos2(resp.rect.right() - 4.0, resp.rect.bottom() + 3.0),
            ],
            Stroke::new(2.0, BORDER),
        );
    }
    resp
}

/// One option of a segmented control (filters).
pub fn segment(ui: &mut Ui, active: bool, label: &str) -> egui::Response {
    let text = if active {
        semibold(label).color(TEXT)
    } else {
        RichText::new(label).color(MUTED)
    };
    let button = egui::Button::new(text)
        .fill(if active {
            tint(ACCENT, 48)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(if active {
            Stroke::new(1.0, tint(ACCENT, 140))
        } else {
            Stroke::NONE
        })
        .corner_radius(CornerRadius::same(6));
    ui.add(button)
}
