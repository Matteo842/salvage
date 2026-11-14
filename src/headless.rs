//! `Salvage.exe --sync`: one full sync with no window, then exit. This is what
//! the Windows Task Scheduler runs, so nothing has to stay open in between.

use std::fmt::Write as _;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::Arc;

use eframe::egui;

use crate::git::{self, Bus, Event};
use crate::model::{self, now_ts, Config};

const TASK: &str = "Salvage sync";

pub fn run() {
    let path = model::config_path();
    let Some(cfg) = Config::load(&path) else {
        return;
    };
    // The window holds this lock while open and syncs on its own schedule.
    let Some(_lock) = model::lock() else {
        return;
    };
    let (tx, rx) = channel();
    let bus = Bus {
        tx,
        ctx: egui::Context::default(),
        cancel: Arc::new(AtomicBool::new(false)),
    };

    let mut log = t!("scheduled sync, {}\n", "sincronizzazione pianificata, {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    let mut results = Vec::new();
    for p in &cfg.projects {
        let result = git::sync(&bus, p, &cfg.root, false);
        for ev in rx.try_iter() {
            if let Event::Log {
                tag,
                text,
                transient: false,
                ..
            } = ev
            {
                let _ = writeln!(log, "{tag:<12} {text}");
            }
        }
        results.push((p.id, result.err()));
    }

    // Re-read before saving: only touch what this run learned.
    let mut cfg = Config::load(&path).unwrap_or(cfg);
    for (id, error) in results {
        if let Some(p) = cfg.projects.iter_mut().find(|p| p.id == id) {
            if error.is_none() {
                p.last_sync = Some(now_ts());
            }
            p.last_error = error;
        }
    }
    cfg.last_sync_all = Some(now_ts());
    cfg.save(&path);
    let _ = std::fs::write(path.with_file_name("last-sync.log"), log);
}

fn schtasks(args: &[&str]) -> Result<(), String> {
    let mut c = Command::new("schtasks");
    c.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = c.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Create or update the scheduled task that runs `--sync` every `hours`.
pub fn schedule(hours: f32) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let run = format!("\"{}\" --sync", exe.display());
    let (unit, every) = if hours < 24.0 {
        ("MINUTE", (hours * 60.0).round().max(1.0) as u32)
    } else {
        ("DAILY", (hours / 24.0).round().max(1.0) as u32)
    };
    schtasks(&["/Create", "/F", "/TN", TASK, "/SC", unit, "/MO", &every.to_string(), "/TR", &run])
}

pub fn unschedule() -> Result<(), String> {
    schtasks(&["/Delete", "/F", "/TN", TASK])
}
