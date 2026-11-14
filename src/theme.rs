//! Palette and egui style. Deep-water salvage: abyss blue, sonar teal, flare orange.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

/// Window background.
pub const ABYSS: Color32 = Color32::from_rgb(5, 13, 20);
/// Opaque base under frosted glass.
pub const GLASS_BASE: Color32 = Color32::from_rgb(7, 18, 27);
/// Glass tint.
pub const HULL: Color32 = Color32::from_rgb(11, 26, 38);
/// Hairlines and borders.
pub const LINE: Color32 = Color32::from_rgb(27, 54, 70);
/// Primary text.
pub const FOAM: Color32 = Color32::from_rgb(207, 232, 238);
/// Secondary text.
pub const KELP: Color32 = Color32::from_rgb(104, 146, 163);
/// Tertiary text.
pub const DEEP: Color32 = Color32::from_rgb(74, 112, 130);
/// Alive / sonar contact.
pub const PING: Color32 = Color32::from_rgb(55, 224, 194);
/// Action / something to fetch.
pub const FLARE: Color32 = Color32::from_rgb(255, 162, 58);
/// Lost signal.
pub const WRECK: Color32 = Color32::from_rgb(255, 90, 110);

pub fn alpha(c: Color32, a: f32) -> Color32 {
    c.gamma_multiply(a.clamp(0.0, 1.0))
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t.clamp(0.0, 1.0))
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, mono(17.0)),
        (TextStyle::Body, mono(12.5)),
        (TextStyle::Monospace, mono(12.0)),
        (TextStyle::Button, mono(12.5)),
        (TextStyle::Small, mono(11.0)),
    ]
    .into();
    style.interaction.selectable_labels = false;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.scroll = egui::style::ScrollStyle::thin();

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = ABYSS;
    v.window_fill = HULL;
    v.extreme_bg_color = ABYSS;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_corner_radius = CornerRadius::same(2);
    v.menu_corner_radius = CornerRadius::same(2);
    v.popup_shadow = egui::Shadow::NONE;
    v.window_shadow = egui::Shadow::NONE;
    v.selection.bg_fill = alpha(PING, 0.28);
    v.selection.stroke = Stroke::new(1.0, PING);
    v.text_cursor.stroke = Stroke::new(2.0, PING);
    v.widgets.noninteractive.fg_stroke.color = FOAM;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.corner_radius = CornerRadius::same(2);
        w.bg_fill = LINE;
        w.weak_bg_fill = LINE;
        w.fg_stroke.color = KELP;
    }
    v.widgets.hovered.bg_fill = DEEP;
    v.widgets.active.bg_fill = KELP;
    ctx.set_style(style);
}
