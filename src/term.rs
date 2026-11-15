//! The live terminal that runs behind the interface: every git command and its
//! output. Painted sharp where nothing covers it and blurred under glass.

use std::collections::VecDeque;
use std::sync::Arc;

use eframe::egui::{self, epaint::TextShape, pos2, text::LayoutJob, Color32, Galley, Painter, Rect, TextFormat};

use crate::git::LogKind;
use crate::theme::*;

pub const ROW_H: f32 = 17.0;
const FONT: f32 = 12.0;
const MAX_LINES: usize = 3000;
const TYPE_SPEED: f64 = 320.0; // characters per second

pub struct Line {
    pub time: String,
    pub tag: String,
    pub kind: LogKind,
    pub text: String,
    /// A progress line that the next update of the same phase overwrites.
    pub transient: bool,
    pub born: f64,
}

#[derive(Default)]
pub struct Term {
    pub lines: VecDeque<Line>,
    /// Lines scrolled back from the newest.
    pub scroll: f32,
}

pub struct Row {
    pub y: f32,
    pub galley: Arc<Galley>,
}

fn phase_of(s: &str) -> &str {
    s.split(':').next().unwrap_or(s)
}

impl Term {
    pub fn push(&mut self, now: f64, tag: &str, kind: LogKind, text: String, transient: bool) {
        let text: String = text.chars().take(240).collect();
        if transient {
            if let Some(last) = self.lines.back_mut() {
                if last.transient && last.tag == tag && phase_of(&last.text) == phase_of(&text) {
                    last.text = text;
                    return;
                }
            }
        }
        self.lines.push_back(Line {
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            tag: tag.to_string(),
            kind,
            text,
            transient,
            born: now,
        });
        if self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
        if self.scroll > 0.0 {
            self.scroll += 1.0; // keep the reader's place while scrolled back
        }
    }

    /// True while the newest lines are still being typed out.
    pub fn typing(&self, now: f64) -> bool {
        self.lines
            .iter()
            .rev()
            .take(60)
            .any(|l| !l.transient && (now - l.born) * TYPE_SPEED < l.text.chars().count() as f64)
    }

    pub fn clamp_scroll(&mut self, visible_rows: f32) {
        let max = (self.lines.len() as f32 - visible_rows).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max);
    }

    /// Lay out the rows that fit in `area`, newest at the bottom.
    pub fn rows(&self, ctx: &egui::Context, area: Rect, now: f64) -> Vec<Row> {
        let font = mono(FONT);
        let fmt = |color: Color32| TextFormat {
            font_id: font.clone(),
            color,
            ..Default::default()
        };
        let skip = self.scroll.floor() as usize;
        let mut y = area.bottom() - 12.0 - ROW_H + self.scroll.fract() * ROW_H;
        let mut out = Vec::new();
        ctx.fonts(|fonts| {
            for (i, line) in self.lines.iter().rev().enumerate().skip(skip) {
                if y < area.top() - ROW_H {
                    break;
                }
                let mut job = LayoutJob::default();
                job.append(&line.time, 0.0, fmt(DEEP()));
                job.append(&format!(" {:<10.10} ", line.tag.to_lowercase()), 0.0, fmt(KELP()));
                let shown = if line.transient {
                    usize::MAX
                } else {
                    ((now - line.born) * TYPE_SPEED).max(0.0) as usize
                };
                let text: String = line.text.chars().take(shown).collect();
                match line.kind {
                    LogKind::Cmd => {
                        job.append("$ ", 0.0, fmt(FLARE()));
                        job.append(&text, 0.0, fmt(FOAM()));
                    }
                    LogKind::Out => job.append(&text, 0.0, fmt(mix(KELP(), FOAM(), 0.25))),
                    LogKind::Err => job.append(&text, 0.0, fmt(WRECK())),
                    LogKind::Info => job.append(&text, 0.0, fmt(FOAM())),
                    LogKind::Good => job.append(&text, 0.0, fmt(PING())),
                }
                if i == 0 && (now * 1.7) as i64 % 2 == 0 {
                    job.append(" \u{2588}", 0.0, fmt(PING()));
                }
                out.push(Row {
                    y,
                    galley: fonts.layout_job(job),
                });
                y -= ROW_H;
            }
        });
        out
    }
}

/// Rows fade out over the top `fade` pixels of `clip`.
pub fn paint_sharp(p: &Painter, rows: &[Row], x: f32, clip: Rect, opacity: f32, fade: f32) {
    if clip.width() < 1.0 || clip.height() < 1.0 {
        return;
    }
    let p = p.with_clip_rect(clip);
    for r in rows {
        if r.y > clip.bottom() || r.y + ROW_H < clip.top() {
            continue;
        }
        let k = if fade > 0.0 {
            ((r.y - clip.top()) / fade).clamp(0.0, 1.0)
        } else {
            1.0
        };
        p.add(TextShape::new(pos2(x, r.y), r.galley.clone(), FOAM()).with_opacity_factor(opacity * k));
    }
}

/// Fake gaussian blur: the same text stamped on two rings of offsets at low
/// opacity. Cheap enough to redo every frame and it follows the live text.
pub fn paint_blur(p: &Painter, rows: &[Row], x: f32, clip: Rect, opacity: f32) {
    const TAPS: [(f32, f32, f32); 15] = [
        (0.0, 0.0, 0.14),
        (2.0, 0.0, 0.09),
        (1.0, 1.73, 0.09),
        (-1.0, 1.73, 0.09),
        (-2.0, 0.0, 0.09),
        (-1.0, -1.73, 0.09),
        (1.0, -1.73, 0.09),
        (4.2, 0.0, 0.05),
        (2.97, 2.97, 0.05),
        (0.0, 4.2, 0.05),
        (-2.97, 2.97, 0.05),
        (-4.2, 0.0, 0.05),
        (-2.97, -2.97, 0.05),
        (0.0, -4.2, 0.05),
        (2.97, -2.97, 0.05),
    ];
    if clip.width() < 1.0 || clip.height() < 1.0 {
        return;
    }
    let p = p.with_clip_rect(clip);
    for r in rows {
        if r.y > clip.bottom() + 4.0 || r.y + ROW_H < clip.top() - 4.0 {
            continue;
        }
        for (dx, dy, w) in TAPS {
            p.add(TextShape::new(pos2(x + dx, r.y + dy), r.galley.clone(), FOAM()).with_opacity_factor(opacity * w));
        }
    }
}
