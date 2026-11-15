//! Hand-painted controls: nothing here uses egui's stock button or checkbox look.

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CursorIcon, Id, Layout, Margin, Painter, Pos2, Rect, Response, RichText,
    Sense, Shape, Stroke, StrokeKind, TextEdit, Ui, UiBuilder,
};

use crate::lang::tr;
use crate::model::{hours_label, INTERVAL_STEPS};
use crate::paint::arc;
use crate::theme::*;

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Sync,
    Scan,
    Plus,
    Minus,
    Edit,
    Folder,
    Term,
    Sliders,
    Close,
    Min,
    Max,
    Stop,
    /// Something to download.
    Down,
    /// A link that no longer answers.
    Skull,
}

/// Draw `icon` centred on `c`; `s` is the half-size in points.
pub fn icon(p: &Painter, c: Pos2, s: f32, icon: Icon, col: Color32) {
    let st = Stroke::new(1.4, col);
    let at = |x: f32, y: f32| c + vec2(x, y) * s;
    let line = |a: (f32, f32), b: (f32, f32)| {
        p.line_segment([at(a.0, a.1), at(b.0, b.1)], st);
    };
    let poly = |pts: &[(f32, f32)], closed: bool| {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| at(x, y)).collect();
        p.add(if closed { Shape::closed_line(pts, st) } else { Shape::line(pts, st) });
    };
    match icon {
        Icon::Sync => {
            for turn in [0.0, std::f32::consts::PI] {
                let (a0, a1) = (turn + 0.5, turn + 2.9);
                p.add(Shape::line(arc(c, s * 0.82, a0, a1), st));
                let tip = c + s * 0.82 * vec2(a1.cos(), a1.sin());
                let dir = vec2(-a1.sin(), a1.cos());
                let nrm = vec2(a1.cos(), a1.sin());
                p.line_segment([tip, tip - dir * s * 0.5 + nrm * s * 0.34], st);
                p.line_segment([tip, tip - dir * s * 0.5 - nrm * s * 0.34], st);
            }
        }
        Icon::Scan => {
            p.circle_stroke(c, s * 0.9, st);
            p.add(Shape::line(arc(c, s * 0.48, -1.9, -0.2), st));
            line((0.0, 0.0), (0.62, -0.62));
            p.circle_filled(c, 1.5, col);
        }
        Icon::Plus => {
            line((-0.75, 0.0), (0.75, 0.0));
            line((0.0, -0.75), (0.0, 0.75));
        }
        Icon::Minus => line((-0.75, 0.0), (0.75, 0.0)),
        Icon::Edit => {
            poly(&[(-0.9, 0.9), (-0.9, 0.4), (0.4, -0.9), (0.9, -0.4), (-0.4, 0.9)], true);
            line((0.1, -0.6), (0.6, -0.1));
        }
        Icon::Folder => poly(
            &[(-0.95, -0.7), (-0.3, -0.7), (-0.05, -0.38), (0.95, -0.38), (0.95, 0.75), (-0.95, 0.75)],
            true,
        ),
        Icon::Term => {
            poly(&[(-0.85, -0.55), (-0.2, 0.0), (-0.85, 0.55)], false);
            line((0.1, 0.6), (0.9, 0.6));
        }
        Icon::Sliders => {
            for (y, knob) in [(-0.6, 0.35), (0.0, -0.4), (0.6, 0.1)] {
                line((-0.9, y), (0.9, y));
                p.circle_filled(at(knob, y), 1.9, ABYSS());
                p.circle_stroke(at(knob, y), 1.9, st);
            }
        }
        Icon::Close => {
            line((-0.6, -0.6), (0.6, 0.6));
            line((-0.6, 0.6), (0.6, -0.6));
        }
        Icon::Min => line((-0.65, 0.5), (0.65, 0.5)),
        Icon::Max => poly(&[(-0.6, -0.6), (0.6, -0.6), (0.6, 0.6), (-0.6, 0.6)], true),
        Icon::Stop => {
            p.rect_filled(Rect::from_center_size(c, vec2(s * 1.2, s * 1.2)), 1, col);
        }
        Icon::Down => {
            line((0.0, -0.85), (0.0, 0.75));
            poly(&[(-0.6, 0.15), (0.0, 0.8), (0.6, 0.15)], false);
        }
        Icon::Skull => {
            p.circle_filled(at(0.0, -0.2), s * 0.86, col);
            p.rect_filled(Rect::from_min_max(at(-0.5, 0.35), at(0.5, 1.02)), 1, col);
            for x in [-0.36, 0.36] {
                p.circle_filled(at(x, -0.12), s * 0.25, GLASS_BASE());
            }
            p.add(Shape::convex_polygon(
                vec![at(0.0, 0.14), at(-0.13, 0.42), at(0.13, 0.42)],
                GLASS_BASE(),
                Stroke::NONE,
            ));
            for x in [-0.18, 0.18] {
                p.line_segment([at(x, 0.66), at(x, 1.04)], Stroke::new(1.0, GLASS_BASE()));
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    /// The one main action on a surface.
    Primary,
    Quiet,
    Danger,
}

fn focus_ring(ui: &Ui, resp: &Response, rect: Rect) {
    if resp.has_focus() {
        ui.painter()
            .rect_stroke(rect.expand(2.0), 3, Stroke::new(1.0, PING()), StrokeKind::Outside);
    }
}

pub fn button(ui: &mut Ui, ic: Option<Icon>, label: &str, tone: Tone) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), mono(12.5), FOAM());
    let icon_w = if ic.is_some() { 20.0 } else { 0.0 };
    let size = vec2(galley.size().x + icon_w + 26.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hot = ui.ctx().animate_bool_with_time(resp.id.with("hot"), resp.hovered(), 0.1);
    let (accent, base) = match tone {
        Tone::Primary => (FLARE(), 0.12),
        Tone::Quiet => (KELP(), 0.0),
        Tone::Danger => (WRECK(), 0.0),
    };
    let text = match tone {
        Tone::Primary => mix(FLARE(), Color32::WHITE, 0.25 * hot),
        Tone::Quiet => mix(KELP(), FOAM(), hot),
        Tone::Danger => WRECK(),
    };
    let border = match tone {
        Tone::Quiet => mix(LINE(), KELP(), hot),
        _ => alpha(accent, 0.55 + 0.45 * hot),
    };
    let p = ui.painter();
    p.rect(
        rect,
        2,
        alpha(accent, base + 0.1 * hot),
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
    let mut x = rect.left() + 13.0;
    if let Some(ic) = ic {
        icon(p, pos2(x + 6.0, rect.center().y), 6.0, ic, text);
        x += icon_w;
    }
    p.galley_with_override_text_color(pos2(x, rect.center().y - galley.size().y / 2.0), galley, text);
    focus_ring(ui, &resp, rect);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Square icon-only button at a fixed place.
pub fn icon_button_at(ui: &mut Ui, rect: Rect, id: Id, ic: Icon, tip: &str, tint: Color32) -> Response {
    let resp = ui.interact(rect, id, Sense::click());
    let hot = ui.ctx().animate_bool_with_time(id.with("hot"), resp.hovered(), 0.1);
    let p = ui.painter();
    if hot > 0.0 {
        p.rect_filled(rect.shrink(1.0), 2, alpha(tint, 0.1 * hot));
    }
    icon(p, rect.center(), 6.5, ic, mix(KELP(), tint, hot));
    focus_ring(ui, &resp, rect);
    resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip)
}

pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(label.to_owned(), mono(12.5), FOAM());
    let size = vec2(30.0 + 12.0 + galley.size().x, 24.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id.with("on"), *on, 0.12);
    let sw = Rect::from_min_size(pos2(rect.left(), rect.center().y - 8.0), vec2(30.0, 16.0));
    let p = ui.painter();
    p.rect(
        sw,
        8,
        mix(alpha(ABYSS(), 0.7), alpha(PING(), 0.18), t),
        Stroke::new(1.0, mix(LINE(), PING(), t)),
        StrokeKind::Inside,
    );
    p.circle_filled(pos2(sw.left() + 8.0 + 14.0 * t, sw.center().y), 4.5, mix(KELP(), PING(), t));
    let col = if resp.hovered() { FOAM() } else { mix(KELP(), FOAM(), 0.4 + 0.6 * t) };
    p.galley_with_override_text_color(pos2(sw.right() + 12.0, rect.center().y - galley.size().y / 2.0), galley, col);
    focus_ring(ui, &resp, rect);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Pick the sync interval from a fixed ladder of sensible values.
pub fn interval_stepper(ui: &mut Ui, hours: &mut f32) {
    let at = INTERVAL_STEPS
        .iter()
        .position(|s| *s >= *hours - 0.001)
        .unwrap_or(INTERVAL_STEPS.len() - 1);
    let (rect, _) = ui.allocate_exact_size(vec2(138.0, 28.0), Sense::hover());
    ui.painter()
        .rect(rect, 2, alpha(ABYSS(), 0.6), Stroke::new(1.0, LINE()), StrokeKind::Inside);
    let left = Rect::from_min_size(rect.min, vec2(28.0, 28.0));
    let right = Rect::from_min_size(pos2(rect.right() - 28.0, rect.top()), vec2(28.0, 28.0));
    if icon_button_at(ui, left, Id::new("interval_less"), Icon::Minus, tr("more often", "più spesso"), FOAM()).clicked() && at > 0 {
        *hours = INTERVAL_STEPS[at - 1];
    }
    if icon_button_at(ui, right, Id::new("interval_more"), Icon::Plus, tr("less often", "meno spesso"), FOAM()).clicked()
        && at + 1 < INTERVAL_STEPS.len()
    {
        *hours = INTERVAL_STEPS[at + 1];
    }
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, hours_label(*hours), mono(12.5), FOAM());
}

/// Pick a whole number between `min` and `max`, one step at a time.
pub fn count_stepper(ui: &mut Ui, id: Id, value: &mut u8, min: u8, max: u8, less: &str, more: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(96.0, 28.0), Sense::hover());
    ui.painter()
        .rect(rect, 2, alpha(ABYSS(), 0.6), Stroke::new(1.0, LINE()), StrokeKind::Inside);
    let left = Rect::from_min_size(rect.min, vec2(28.0, 28.0));
    let right = Rect::from_min_size(pos2(rect.right() - 28.0, rect.top()), vec2(28.0, 28.0));
    let dim = |on: bool| if on { FOAM() } else { DEEP() };
    if icon_button_at(ui, left, id.with("less"), Icon::Minus, less, dim(*value > min)).clicked() && *value > min {
        *value -= 1;
    }
    if icon_button_at(ui, right, id.with("more"), Icon::Plus, more, dim(*value < max)).clicked() && *value < max {
        *value += 1;
    }
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, value.to_string(), mono(12.5), FOAM());
}

/// Single-line text field with our own frame.
pub fn field(ui: &mut Ui, id: Id, text: &mut String, hint: &str, width: f32) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 32.0), Sense::hover());
    let bg = ui.painter().add(Shape::Noop);
    let inner = rect.shrink2(vec2(10.0, 0.0));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let resp = child.add(
        TextEdit::singleline(text)
            .id(id)
            .frame(false)
            .margin(Margin::ZERO)
            .desired_width(inner.width())
            .font(mono(12.5))
            .text_color(FOAM())
            .hint_text(RichText::new(hint).color(DEEP())),
    );
    let stroke = if resp.has_focus() { PING() } else { LINE() };
    ui.painter().set(
        bg,
        egui::epaint::RectShape::new(rect, 2, alpha(ABYSS(), 0.65), Stroke::new(1.0, stroke), StrokeKind::Inside),
    );
    resp
}

/// Text that behaves like a link.
pub fn link(ui: &mut Ui, id: Id, at: Pos2, text: String, size: f32, idle: Color32) -> Response {
    let galley = ui.painter().layout_no_wrap(text, mono(size), idle);
    let rect = Rect::from_min_size(at, galley.size());
    let resp = ui.interact(rect, id, Sense::click());
    let col = if resp.hovered() { FOAM() } else { idle };
    ui.painter().galley_with_override_text_color(at, galley, col);
    if resp.hovered() {
        ui.painter()
            .line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, alpha(FOAM(), 0.5)));
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// One entry of a [`dropdown`]: what it says, and a dimmer line under it.
pub struct Pick {
    pub label: String,
    pub detail: String,
}

/// Field-shaped button that opens a list of [`Pick`]s below it.
/// Returns the index of the entry clicked, if any.
pub fn dropdown(ui: &mut Ui, id: Id, shown: &str, width: f32, items: &[Pick]) -> Option<usize> {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
    let mut open = ui.memory(|m| m.is_popup_open(id));
    if resp.clicked() {
        ui.memory_mut(|m| m.toggle_popup(id));
        open = !open;
    }
    let hot = resp.hovered() || open;
    let p = ui.painter();
    p.rect(rect, 2, alpha(ABYSS(), 0.65), Stroke::new(1.0, if hot { PING() } else { LINE() }), StrokeKind::Inside);
    p.text(pos2(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, shown, mono(12.5), FOAM());
    // Chevron: points down when closed, up when open.
    let c = pos2(rect.right() - 16.0, rect.center().y);
    let dy = if open { -2.5 } else { 2.5 };
    let col = if hot { FOAM() } else { KELP() };
    p.line_segment([pos2(c.x - 5.0, c.y - dy), pos2(c.x, c.y + dy)], Stroke::new(1.5, col));
    p.line_segment([pos2(c.x, c.y + dy), pos2(c.x + 5.0, c.y - dy)], Stroke::new(1.5, col));
    focus_ring(ui, &resp, rect);
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if !open {
        return None;
    }

    // Tooltip order keeps the list above the dialog it belongs to.
    let mut picked = None;
    let area = egui::Area::new(id.with("list"))
        .order(egui::Order::Tooltip)
        .fixed_pos(rect.left_bottom() + vec2(0.0, 4.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(HULL())
                .stroke(Stroke::new(1.0, PING()))
                .corner_radius(2)
                .inner_margin(Margin::same(4))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
                    let w = width - 10.0;
                    let chars = (w / 7.2) as usize;
                    for (i, it) in items.iter().enumerate() {
                        let h = if it.detail.is_empty() { 28.0 } else { 44.0 };
                        let (r, row) = ui.allocate_exact_size(vec2(w, h), Sense::click());
                        let p = ui.painter();
                        if row.hovered() {
                            p.rect_filled(r, 2, alpha(PING(), 0.12));
                        }
                        let top = if it.detail.is_empty() { r.center().y } else { r.top() + 14.0 };
                        p.text(pos2(r.left() + 8.0, top), Align2::LEFT_CENTER, &it.label, mono(12.5), FOAM());
                        if !it.detail.is_empty() {
                            let mut d: String = it.detail.chars().take(chars).collect();
                            if it.detail.chars().count() > chars {
                                d.pop();
                                d.push('\u{2026}');
                            }
                            p.text(pos2(r.left() + 8.0, r.top() + 31.0), Align2::LEFT_CENTER, d, mono(11.0), KELP());
                        }
                        if row.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                            picked = Some(i);
                        }
                    }
                });
        });
    let outside = ui.input(|i| i.pointer.any_click())
        && !resp.clicked()
        && !ui.input(|i| i.pointer.interact_pos()).is_some_and(|q| area.response.rect.contains(q));
    if picked.is_some() || outside || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ui.memory_mut(|m| m.close_popup());
    }
    picked
}
