//! Code-drawn graphics: the ASCII wordmark, the sonar scope, status pings,
//! frosted glass and the window icon.

use std::f32::consts::TAU;

use eframe::egui::{
    self, epaint::Mesh, pos2, text::LayoutJob, vec2, Align2, Color32, Painter, Pos2, Rect, Shape, Stroke, StrokeKind,
    TextFormat,
};

use crate::term::{self, Row};
use crate::theme::*;

/// What a repo's indicator is showing.
#[derive(Clone, Copy, PartialEq)]
pub enum Sig {
    Unknown,
    Live,
    Behind,
    Lost,
    Listening,
    Working,
}

impl Sig {
    pub fn color(self) -> Color32 {
        match self {
            Sig::Live => PING,
            Sig::Behind | Sig::Working => FLARE,
            Sig::Lost => WRECK,
            Sig::Unknown | Sig::Listening => KELP,
        }
    }
}

/// Stable pseudo-random value in 0..1 from an id.
pub fn hash01(seed: u64, salt: u64) -> f32 {
    let mut z = seed.wrapping_add(salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 40) as f32 / (1u64 << 24) as f32
}

pub fn arc(c: Pos2, r: f32, a0: f32, a1: f32) -> Vec<Pos2> {
    let n = (((a1 - a0).abs() / TAU) * 64.0).ceil().max(2.0) as usize;
    (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / n as f32;
            c + r * vec2(a.cos(), a.sin())
        })
        .collect()
}

/// Per-repo status indicator.
pub fn ping(p: &Painter, c: Pos2, sig: Sig, now: f64, seed: u64) {
    let col = sig.color();
    match sig {
        Sig::Live | Sig::Behind => {
            let t = (now as f32 * 0.42 + hash01(seed, 7)).fract();
            p.circle_stroke(c, 4.0 + 11.0 * t, Stroke::new(1.0, alpha(col, (1.0 - t).powi(2) * 0.75)));
            p.circle_filled(c, 3.2, col);
        }
        Sig::Lost => {
            p.circle_stroke(c, 6.0, Stroke::new(1.3, col));
            p.line_segment([c + vec2(-4.2, 4.2), c + vec2(4.2, -4.2)], Stroke::new(1.3, col));
        }
        Sig::Unknown => {
            p.circle_stroke(c, 3.5, Stroke::new(1.2, DEEP));
        }
        Sig::Listening => {
            let a = now as f32 * 3.2;
            p.circle_filled(c, 2.2, col);
            p.add(Shape::line(arc(c, 7.5, a, a + 1.9), Stroke::new(1.3, col)));
        }
        Sig::Working => {
            let a = now as f32 * 4.4;
            p.circle_filled(c, 3.0, col);
            p.add(Shape::line(arc(c, 8.0, a, a + 1.5), Stroke::new(1.4, col)));
            p.add(Shape::line(arc(c, 8.0, a + TAU / 2.0, a + TAU / 2.0 + 1.5), Stroke::new(1.4, col)));
        }
    }
}

pub struct Blip {
    pub seed: u64,
    pub name: String,
    pub sig: Sig,
}

/// Sonar scope: one contact per repo, lit as the sweep passes over it.
/// `countdown` is how far we are towards the next automatic run (0..1).
pub fn scope(p: &Painter, c: Pos2, r: f32, now: f64, blips: &[Blip], countdown: Option<f32>) {
    for k in 1..=3 {
        let a = if k == 3 { 1.0 } else { 0.55 };
        p.circle_stroke(c, r * k as f32 / 3.0, Stroke::new(1.0, alpha(LINE, a)));
    }
    let cross = Stroke::new(1.0, alpha(LINE, 0.5));
    p.line_segment([c - vec2(r, 0.0), c + vec2(r, 0.0)], cross);
    p.line_segment([c - vec2(0.0, r), c + vec2(0.0, r)], cross);

    let sweep = (now / 4.8).fract() as f32 * TAU;

    // Trailing glow behind the sweep line.
    let mut mesh = Mesh::default();
    let steps = 28;
    let trail = 1.25;
    for i in 0..steps {
        let a0 = sweep - trail * i as f32 / steps as f32;
        let a1 = sweep - trail * (i + 1) as f32 / steps as f32;
        let k = 1.0 - i as f32 / steps as f32;
        let col = alpha(PING, 0.22 * k * k);
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(c, col);
        mesh.colored_vertex(c + r * vec2(a0.cos(), a0.sin()), col);
        mesh.colored_vertex(c + r * vec2(a1.cos(), a1.sin()), col);
        mesh.add_triangle(base, base + 1, base + 2);
    }
    p.add(Shape::mesh(mesh));
    p.line_segment([c, c + r * vec2(sweep.cos(), sweep.sin())], Stroke::new(1.2, alpha(PING, 0.85)));

    for b in blips {
        // Golden-angle spacing keeps contacts apart however many there are.
        let theta = (b.seed as f32 * 2.399_963 + 0.9).rem_euclid(TAU);
        let dist = r * (0.36 + 0.5 * (b.seed as f32 * 0.618_034 + 0.2).fract());
        let pos = c + dist * vec2(theta.cos(), theta.sin());
        let since = (sweep - theta).rem_euclid(TAU) / TAU; // 0 = just swept
        let lit = (1.0 - since).powi(3);
        let col = b.sig.color();
        if since < 0.3 {
            let t = since / 0.3;
            p.circle_stroke(pos, 3.0 + 10.0 * t, Stroke::new(1.0, alpha(col, (1.0 - t) * 0.6)));
        }
        if b.sig == Sig::Lost {
            let s = Stroke::new(1.4, alpha(col, 0.45 + 0.55 * lit));
            p.line_segment([pos + vec2(-3.0, -3.0), pos + vec2(3.0, 3.0)], s);
            p.line_segment([pos + vec2(-3.0, 3.0), pos + vec2(3.0, -3.0)], s);
        } else {
            p.circle_filled(pos, 2.2 + 1.4 * lit, alpha(col, 0.4 + 0.6 * lit));
        }
        let name: String = b.name.chars().take(12).collect::<String>().to_lowercase();
        let (anchor, off) = if pos.x > c.x + r * 0.35 {
            (Align2::RIGHT_CENTER, vec2(-8.0, 0.0))
        } else {
            (Align2::LEFT_CENTER, vec2(8.0, 0.0))
        };
        p.text(pos + off, anchor, name, mono(9.5), alpha(mix(KELP, FOAM, lit), 0.35 + 0.65 * lit));
    }

    p.circle_filled(c, 1.8, PING);

    // Outer ring: time until the next automatic run.
    let ring = r + 9.0;
    p.circle_stroke(c, ring, Stroke::new(1.0, alpha(LINE, 0.6)));
    if let Some(f) = countdown {
        let f = f.clamp(0.0, 1.0);
        if f > 0.002 {
            p.add(Shape::line(arc(c, ring, -TAU / 4.0, -TAU / 4.0 + TAU * f), Stroke::new(1.6, FLARE)));
        }
        let a = -TAU / 4.0 + TAU * f;
        p.circle_filled(c + ring * vec2(a.cos(), a.sin()), 2.4, FLARE);
    }
}

const LETTERS: [[&str; 6]; 7] = [
    ["███████╗", "██╔════╝", "███████╗", "╚════██║", "███████║", "╚══════╝"],
    [" █████╗ ", "██╔══██╗", "███████║", "██╔══██║", "██║  ██║", "╚═╝  ╚═╝"],
    ["██╗     ", "██║     ", "██║     ", "██║     ", "███████╗", "╚══════╝"],
    ["██╗   ██╗", "██║   ██║", "██║   ██║", "╚██╗ ██╔╝", " ╚████╔╝ ", "  ╚═══╝  "],
    [" █████╗ ", "██╔══██╗", "███████║", "██╔══██║", "██║  ██║", "╚═╝  ╚═╝"],
    [" ██████╗ ", "██╔════╝ ", "██║  ███╗", "██║   ██║", "╚██████╔╝", " ╚═════╝ "],
    ["███████╗", "██╔════╝", "█████╗  ", "██╔══╝  ", "███████╗", "╚══════╝"],
];

/// ASCII wordmark, scaled to `width`. Returns the y just below it.
pub fn logo(p: &Painter, at: Pos2, width: f32) -> f32 {
    let cols: usize = LETTERS.iter().map(|l| l[0].chars().count()).sum();
    let probe = mono(10.0);
    let (w10, _) = p.fonts(|f| (f.glyph_width(&probe, '\u{2588}'), f.row_height(&probe)));
    let size = (10.0 * width / (w10 * cols as f32)).min(11.0);
    let font = mono(size);
    let row_h = p.fonts(|f| f.row_height(&font));
    let mut y = at.y;
    for row in 0..6 {
        let line: String = LETTERS.iter().map(|l| l[row]).collect();
        let face = mix(FOAM, PING, row as f32 / 5.0);
        let mut job = LayoutJob::default();
        let mut run = String::new();
        let mut run_block = true;
        let flush = |job: &mut LayoutJob, run: &mut String, block: bool| {
            if !run.is_empty() {
                job.append(
                    run,
                    0.0,
                    TextFormat {
                        font_id: font.clone(),
                        color: if block { face } else { DEEP },
                        ..Default::default()
                    },
                );
                run.clear();
            }
        };
        for ch in line.chars() {
            let block = ch == '\u{2588}' || ch == ' ';
            if block != run_block && ch != ' ' {
                flush(&mut job, &mut run, run_block);
                run_block = block;
            }
            run.push(ch);
        }
        flush(&mut job, &mut run, run_block);
        let galley = p.layout_job(job);
        p.galley(pos2(at.x, y), galley, FOAM);
        y += row_h;
    }
    y
}

/// Frosted pane: opaque base, the terminal blurred through it, a tint, a hairline.
pub fn glass(p: &Painter, rect: Rect, rows: &[Row], term_x: f32) {
    p.rect_filled(rect, 3, GLASS_BASE);
    term::paint_blur(p, rows, term_x, rect.shrink(1.0).intersect(p.clip_rect()), 0.66);
    p.rect_filled(rect, 3, alpha(HULL, 0.5));
    p.rect_stroke(rect, 3, Stroke::new(1.0, LINE), StrokeKind::Inside);
    p.line_segment(
        [pos2(rect.left() + 3.0, rect.top() + 1.5), pos2(rect.right() - 3.0, rect.top() + 1.5)],
        Stroke::new(1.0, alpha(FOAM, 0.06)),
    );
}

/// Window / taskbar icon. The same pixels are baked into the .exe by build.rs.
pub fn app_icon() -> egui::IconData {
    egui::IconData {
        rgba: crate::icon::icon_rgba(64),
        width: 64,
        height: 64,
    }
}
