//! Persisted state: the list of repos and the app settings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lang::tr;

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Project {
    pub id: u64,
    /// Display name, also the folder name under the backup root.
    pub name: String,
    pub url: String,
    /// Keep a bare `--mirror` clone (every branch and tag).
    #[serde(default = "yes")]
    pub mirror: bool,
    /// Keep a normal checkout with submodules.
    #[serde(default = "yes")]
    pub work: bool,
    #[serde(default)]
    pub last_sync: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Links this repo used before, newest first.
    #[serde(default)]
    pub old_urls: Vec<String>,
}

impl Project {
    pub fn mirror_dir(&self, root: &Path) -> PathBuf {
        root.join(format!("{}-mirror.git", self.name))
    }

    pub fn work_dir(&self, root: &Path) -> PathBuf {
        root.join(&self.name)
    }
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub root: PathBuf,
    pub interval_hours: f32,
    pub auto_sync: bool,
    pub check_on_start: bool,
    #[serde(default)]
    pub last_sync_all: Option<i64>,
    /// A Windows scheduled task runs `--sync` while the window is closed.
    #[serde(default)]
    pub scheduled_task: bool,
    pub next_id: u64,
    pub projects: Vec<Project>,
}

impl Config {
    pub fn fresh(root: PathBuf) -> Self {
        Self {
            root,
            interval_hours: 6.0,
            auto_sync: true,
            check_on_start: true,
            last_sync_all: Some(now_ts()),
            scheduled_task: false,
            next_id: 1,
            projects: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Write through a temp file so a crash mid-write cannot corrupt the list.
    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    pub fn add(&mut self, name: String, url: String, mirror: bool, work: bool) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.projects.push(Project {
            id,
            name,
            url,
            mirror,
            work,
            last_sync: None,
            last_error: None,
            old_urls: Vec::new(),
        });
        id
    }

    pub fn has_name(&self, name: &str) -> bool {
        self.projects
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn interval_secs(&self) -> i64 {
        (self.interval_hours as f64 * 3600.0) as i64
    }
}

/// `SALVAGE_CONFIG` overrides the location (used for testing against a throwaway root).
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SALVAGE_CONFIG") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(Path::to_path_buf))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Salvage").join("config.json")
}

/// Held by whichever Salvage is working (window or `--sync`), so the two never
/// sync the same folders at once. `None` means the other one has it.
pub fn lock() -> Option<std::fs::File> {
    let path = config_path().with_file_name("salvage.lock");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut open = std::fs::OpenOptions::new();
    open.create(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open.share_mode(0);
    }
    open.open(path).ok()
}

pub fn default_root() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|h| PathBuf::from(h).join("Salvage"))
        .unwrap_or_else(|| PathBuf::from("Salvage"))
}

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Last path segment of a git link, without `.git`, safe to use as a folder name.
pub fn name_from_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let seg = u.rsplit(['/', ':']).next().unwrap_or("");
    sanitize_name(seg)
}

pub fn sanitize_name(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') && !c.is_control())
        .collect::<String>()
        .trim_matches(['.', ' '])
        .to_string()
}

/// `https://github.com/a/b.git` -> `github.com/a/b`
pub fn short_url(url: &str) -> String {
    let u = url.trim();
    let u = u.split_once("://").map_or(u, |(_, rest)| rest);
    let u = u.strip_prefix("git@").unwrap_or(u);
    let u = u.trim_end_matches('/');
    u.strip_suffix(".git").unwrap_or(u).to_string()
}

/// The page to open in a browser for this link, if it is a web link.
pub fn web_url(url: &str) -> Option<String> {
    let u = url.trim();
    if u.starts_with("http://") || u.starts_with("https://") {
        Some(u.strip_suffix(".git").unwrap_or(u).to_string())
    } else {
        None
    }
}

pub fn same_url(a: &str, b: &str) -> bool {
    short_url(a).eq_ignore_ascii_case(&short_url(b))
}

pub fn ago(ts: i64, now: i64) -> String {
    let d = (now - ts).max(0);
    if d < 60 {
        tr("just now", "adesso").into()
    } else if d < 3600 {
        t!("{} min ago", "{} min fa", d / 60)
    } else if d < 48 * 3600 {
        t!("{} h ago", "{} h fa", d / 3600)
    } else {
        t!("{} days ago", "{} giorni fa", d / 86400)
    }
}

pub fn hours_label(h: f32) -> String {
    if h < 1.0 {
        format!("{} min", (h * 60.0).round() as i32)
    } else if h >= 48.0 && (h % 24.0) == 0.0 {
        t!("{} days", "{} giorni", (h / 24.0) as i32)
    } else {
        format!("{} h", h as i32)
    }
}

pub const INTERVAL_STEPS: [f32; 12] = [0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 24.0, 48.0, 72.0, 168.0];
