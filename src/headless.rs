//! `salvage --sync`: one full sync with no window, then exit. This is what the
//! scheduler runs (Windows Task Scheduler, or a systemd user timer on Linux),
//! so nothing has to stay open in between.

use std::fmt::Write as _;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::Arc;

use eframe::egui;

use crate::git::{self, Bus, Event};
use crate::model::{self, now_ts, Config};

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

/// Run a scheduler command; on failure, its own words.
fn run_tool(program: &str, args: &[&str]) -> Result<(), String> {
    let mut c = Command::new(program);
    c.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = c.output().map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(windows)]
mod job {
    use super::run_tool;
    use crate::lang::tr;

    const TASK: &str = "Salvage sync";

    pub fn schedule(hours: f32) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let run = format!("\"{}\" --sync", exe.display());
        let (unit, every) = if hours < 24.0 {
            ("MINUTE", (hours * 60.0).round().max(1.0) as u32)
        } else {
            ("DAILY", (hours / 24.0).round().max(1.0) as u32)
        };
        run_tool("schtasks", &["/Create", "/F", "/TN", TASK, "/SC", unit, "/MO", &every.to_string(), "/TR", &run])
    }

    pub fn unschedule() -> Result<(), String> {
        run_tool("schtasks", &["/Delete", "/F", "/TN", TASK])
    }

    pub fn name() -> &'static str {
        tr("Task Scheduler", "Utilità di pianificazione")
    }

    pub fn tool() -> &'static str {
        tr("Windows Task Scheduler", "l'Utilità di pianificazione di Windows")
    }

    pub fn starter() -> &'static str {
        "Windows"
    }

    pub fn set() -> &'static str {
        tr("Windows scheduled task set: every", "attività pianificata in Windows: ogni")
    }

    pub fn removed() -> &'static str {
        tr("Windows scheduled task removed", "attività pianificata rimossa da Windows")
    }
}

/// A systemd user timer: `~/.config/systemd/user/salvage-sync.{service,timer}`.
#[cfg(not(windows))]
mod job {
    use std::path::PathBuf;

    use super::run_tool;
    use crate::lang::tr;

    const UNIT: &str = "salvage-sync";

    fn unit_dir() -> Result<PathBuf, String> {
        crate::model::config_home()
            .map(|c| c.join("systemd").join("user"))
            .ok_or_else(|| "HOME is not set".to_string())
    }

    fn systemctl(args: &[&str]) -> Result<(), String> {
        let mut all = vec!["--user"];
        all.extend_from_slice(args);
        run_tool("systemctl", &all)
    }

    pub fn schedule(hours: f32) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // systemd expands `%` specifiers, so a literal one is written twice.
        let exe = exe.display().to_string().replace('%', "%%");
        let minutes = (hours * 60.0).round().max(1.0) as u32;
        let dir = unit_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let service = format!(
            "[Unit]
Description=Salvage: sync the repo backups

[Service]
Type=oneshot
ExecStart=\"{exe}\" --sync
"
        );
        // First run one interval after the timer starts (login), then one
        // interval after each run.
        let timer = format!(
            "[Unit]
Description=Salvage: sync the repo backups every {minutes} minutes

[Timer]
OnActiveSec={minutes}min
OnUnitActiveSec={minutes}min

[Install]
WantedBy=timers.target
"
        );
        std::fs::write(dir.join(format!("{UNIT}.service")), service).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(format!("{UNIT}.timer")), timer).map_err(|e| e.to_string())?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", &format!("{UNIT}.timer")])?;
        // Restart so a changed interval takes effect now.
        systemctl(&["restart", &format!("{UNIT}.timer")])
    }

    pub fn unschedule() -> Result<(), String> {
        let stop = systemctl(&["disable", "--now", &format!("{UNIT}.timer")]);
        let dir = unit_dir()?;
        let _ = std::fs::remove_file(dir.join(format!("{UNIT}.timer")));
        let _ = std::fs::remove_file(dir.join(format!("{UNIT}.service")));
        let _ = systemctl(&["daemon-reload"]);
        stop
    }

    pub fn name() -> &'static str {
        "systemd"
    }

    pub fn tool() -> &'static str {
        tr("a systemd user timer", "un timer utente di systemd")
    }

    pub fn starter() -> &'static str {
        "systemd"
    }

    pub fn set() -> &'static str {
        tr("systemd timer set: every", "timer di systemd attivato: ogni")
    }

    pub fn removed() -> &'static str {
        tr("systemd timer removed", "timer di systemd rimosso")
    }
}

/// Create or update the scheduled job that runs `--sync` every `hours`.
pub fn schedule(hours: f32) -> Result<(), String> {
    job::schedule(hours)
}

pub fn unschedule() -> Result<(), String> {
    job::unschedule()
}

/// Short scheduler name, used before its error messages.
pub fn scheduler_text() -> &'static str {
    job::name()
}

/// Log line when the job is set; the interval follows.
pub fn set_text() -> &'static str {
    job::set()
}

pub fn removed_text() -> &'static str {
    job::removed()
}

pub fn toggle_text() -> String {
    t!("also while the app is closed, via {}", "anche ad app chiusa, con {}", job::tool())
}

pub fn note_text() -> String {
    t!(
        "While closed, {} starts Salvage with no window, syncs and quits: nothing stays running.",
        "Ad app chiusa {} avvia Salvage senza finestra, sincronizza e lo richiude: niente resta acceso.",
        job::starter()
    )
}
