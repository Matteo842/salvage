//! Everything that shells out to `git`. Runs on worker threads and reports back
//! through [`Bus`]; nothing here touches the UI state directly.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::lang::{branches, tr};
use crate::model::{same_url, Project};

/// Error text of a sync the user stopped; the app treats it as "not a failure".
pub fn stopped_msg() -> &'static str {
    tr("stopped", "interrotto")
}

#[derive(Clone, Copy, PartialEq)]
pub enum LogKind {
    Cmd,
    Out,
    Err,
    Info,
    Good,
}

/// Result of asking a remote whether it is still there.
#[derive(Clone)]
pub struct Check {
    pub live: bool,
    pub error: Option<String>,
    pub head: Option<String>,
    pub branches: usize,
    /// Remote branches whose tip differs from what is on disk.
    pub behind: usize,
    /// Something has already been downloaded for this repo.
    pub cloned: bool,
}

pub struct Found {
    pub name: String,
    pub url: String,
    pub mirror: bool,
    pub work: bool,
    /// When git last fetched into this folder, if it ever did.
    pub last_sync: Option<i64>,
}

fn last_fetch(dir: &Path, bare: bool) -> Option<i64> {
    let git_dir = if bare { dir.to_path_buf() } else { dir.join(".git") };
    let stamp = ["FETCH_HEAD", "HEAD"]
        .iter()
        .find_map(|f| std::fs::metadata(git_dir.join(f)).ok())?
        .modified()
        .ok()?;
    let secs = stamp.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(secs as i64)
}

pub enum Event {
    Log {
        tag: String,
        kind: LogKind,
        text: String,
        transient: bool,
    },
    Progress {
        id: u64,
        phase: String,
        pct: f32,
    },
    Checked {
        id: u64,
        check: Check,
    },
    Synced {
        id: u64,
        error: Option<String>,
    },
    Tested {
        url: String,
        ok: bool,
        detail: String,
    },
    Imported(Vec<Found>),
}

#[derive(Clone)]
pub struct Bus {
    pub tx: Sender<Event>,
    pub ctx: egui::Context,
    /// Set by "ferma": every running git child is killed.
    pub cancel: Arc<AtomicBool>,
}

impl Bus {
    pub fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }

    pub fn log(&self, tag: &str, kind: LogKind, text: impl Into<String>) {
        self.send(Event::Log {
            tag: tag.to_string(),
            kind,
            text: text.into(),
            transient: false,
        });
    }
}

fn git(dir: Option<&Path>) -> Command {
    let mut c = Command::new("git");
    if let Some(d) = dir {
        c.current_dir(d);
    }
    // A dead GitHub repo answers with an auth challenge: never let git or the
    // credential manager pop a login window for it.
    c.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Quick silent query, e.g. reading a config value.
fn query(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git(Some(dir)).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn clean(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes).replace("\x1b[K", "");
    s.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Git redraws progress with `\r`, so split on both `\r` and `\n`.
fn read_segments(mut r: impl Read, mut f: impl FnMut(String)) {
    let mut buf = [0u8; 4096];
    let mut seg: Vec<u8> = Vec::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                if !seg.is_empty() {
                    f(clean(&seg));
                    seg.clear();
                }
            } else {
                seg.push(b);
            }
        }
    }
    if !seg.is_empty() {
        f(clean(&seg));
    }
}

/// `Receiving objects:  45% (450/1000), 1.2 MiB` -> ("Receiving objects", 0.45)
fn parse_progress(s: &str) -> Option<(String, f32)> {
    let pct_at = s.find('%')?;
    let colon = s[..pct_at].rfind(':')?;
    let pct: f32 = s[colon + 1..pct_at].trim().parse().ok()?;
    let phase = s[..colon].trim().trim_start_matches("remote:").trim();
    Some((phase.to_string(), (pct / 100.0).clamp(0.0, 1.0)))
}

fn is_error_line(s: &str) -> bool {
    let l = s.trim_start_matches("remote:").trim_start();
    l.starts_with("fatal:") || l.starts_with("error:") || l.starts_with("Repository not found")
}

struct Done {
    ok: bool,
    stdout: String,
    err: String,
}

/// Run one git command, streaming its output to the terminal.
fn run(
    bus: &Bus,
    tag: &str,
    id: Option<u64>,
    dir: Option<&Path>,
    args: &[&str],
    timeout: Option<Duration>,
    echo_stdout: bool,
) -> Done {
    bus.log(tag, LogKind::Cmd, format!("git {}", args.join(" ")));
    let mut child = match git(dir).args(args).spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = t!("could not start git: {e}", "git non parte: {e}");
            bus.log(tag, LogKind::Err, msg.clone());
            return Done {
                ok: false,
                stdout: String::new(),
                err: msg,
            };
        }
    };
    let out = child.stdout.take().expect("piped stdout");
    let err = child.stderr.take().expect("piped stderr");

    let t_out = std::thread::spawn({
        let bus = bus.clone();
        let tag = tag.to_string();
        move || {
            let mut all = String::new();
            read_segments(out, |line| {
                if echo_stdout {
                    bus.log(&tag, LogKind::Out, line.clone());
                }
                all.push_str(&line);
                all.push('\n');
            });
            all
        }
    });
    let t_err = std::thread::spawn({
        let bus = bus.clone();
        let tag = tag.to_string();
        move || {
            let mut last_err = String::new();
            let mut last_line = String::new();
            read_segments(err, |line| {
                if line.is_empty() {
                    return;
                }
                if let Some((phase, pct)) = parse_progress(&line) {
                    if let Some(id) = id {
                        bus.send(Event::Progress { id, phase, pct });
                    }
                    bus.send(Event::Log {
                        tag: tag.clone(),
                        kind: LogKind::Out,
                        text: line,
                        transient: true,
                    });
                    return;
                }
                let bad = is_error_line(&line);
                if bad {
                    last_err = line.clone();
                }
                last_line = line.clone();
                bus.log(&tag, if bad { LogKind::Err } else { LogKind::Out }, line);
            });
            if last_err.is_empty() {
                last_line
            } else {
                last_err
            }
        }
    });

    let start = Instant::now();
    let mut killed: Option<&str> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => break None,
        }
        if bus.cancel.load(Ordering::Relaxed) {
            killed = Some(stopped_msg());
        } else if timeout.is_some_and(|t| start.elapsed() > t) {
            killed = Some(tr("no answer before the time limit", "nessuna risposta entro il tempo limite"));
        }
        if killed.is_some() {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(40));
    };

    if let Some(why) = killed {
        // Helper processes may still hold the pipes: do not wait on the readers.
        bus.log(tag, LogKind::Err, why);
        return Done {
            ok: false,
            stdout: String::new(),
            err: why.to_string(),
        };
    }
    let stdout = t_out.join().unwrap_or_default();
    let err = t_err.join().unwrap_or_default();
    Done {
        ok: status.is_some_and(|s| s.success()),
        stdout,
        err,
    }
}

/// Turn git's wording into something a person can act on.
fn humanize(err: &str) -> String {
    let e = err.trim();
    let low = e.to_lowercase();
    if low.contains("repository not found") || low.contains("not found") && low.contains("repository") {
        tr("repository not found (removed or made private)", "repository non trovato (rimosso o reso privato)").into()
    } else if low.contains("could not resolve host") {
        tr("host unreachable (domain gone or no network)", "host non raggiungibile (dominio sparito o niente rete)").into()
    } else if low.contains("could not read username") || low.contains("authentication failed") {
        tr("server asks for a login (removed or made private)", "il server chiede un login (rimosso o reso privato)").into()
    } else if low.contains("failed to connect") || low.contains("connection timed out") || low.contains("connection refused") {
        tr("server does not answer", "il server non risponde").into()
    } else if low.contains("fast-forward") || low.contains("unrelated histories") || low.contains("divergent") {
        tr("working copy no longer follows the remote: open edit and re-clone it", "la copia di lavoro non segue più il remoto: apri modifica e riparti da zero").into()
    } else if e.is_empty() {
        tr("git failed without saying why", "git ha fallito senza spiegazioni").into()
    } else {
        e.trim_start_matches("remote:")
            .trim()
            .trim_start_matches("fatal:")
            .trim_start_matches("error:")
            .trim()
            .to_string()
    }
}

fn is_repo(dir: &Path) -> bool {
    dir.join("HEAD").is_file() || dir.join(".git").exists()
}

fn short(hash: &str) -> String {
    hash.chars().take(7).collect()
}

/// Ask the remote for its refs and compare them with what is on disk.
pub fn check(bus: &Bus, p: &Project, root: &Path) -> Check {
    let tag = p.name.as_str();
    let mirror = p.mirror_dir(root);
    let work = p.work_dir(root);
    let cloned = is_repo(&mirror) || is_repo(&work);

    let done = run(bus, tag, None, None, &["ls-remote", &p.url], Some(Duration::from_secs(40)), false);
    if !done.ok {
        let error = humanize(&done.err);
        bus.log(tag, LogKind::Err, t!("no answer: {error}", "non risponde: {error}"));
        return Check {
            live: false,
            error: Some(error),
            head: None,
            branches: 0,
            behind: 0,
            cloned,
        };
    }

    let mut head = None;
    let mut heads: HashMap<&str, &str> = HashMap::new();
    for line in done.stdout.lines() {
        let Some((hash, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim();
        if name == "HEAD" {
            head = Some(hash);
        } else if let Some(b) = name.strip_prefix("refs/heads/") {
            heads.insert(b, hash);
        }
    }

    let mut behind = 0;
    if is_repo(&mirror) {
        let local = query(&mirror, &["for-each-ref", "--format=%(objectname) %(refname)", "refs/heads"]).unwrap_or_default();
        let local: HashMap<&str, &str> = local
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter_map(|(h, r)| r.strip_prefix("refs/heads/").map(|b| (b, h)))
            .collect();
        behind = heads.iter().filter(|(b, h)| local.get(*b) != Some(*h)).count();
    } else if is_repo(&work) {
        let local = query(&work, &["rev-parse", "HEAD"]).unwrap_or_default();
        let branch = query(&work, &["symbolic-ref", "--short", "-q", "HEAD"]).unwrap_or_default();
        let remote = heads.get(branch.as_str()).copied().or(head).unwrap_or("");
        behind = usize::from(local != remote);
    }

    let summary = if !cloned {
        t!("online, {}, not downloaded yet", "online, {}, ancora da scaricare", branches(heads.len()))
    } else if behind > 0 {
        t!("online, {}, {behind} to update", "online, {}, {behind} da aggiornare", branches(heads.len()))
    } else {
        t!("online, {}, up to date", "online, {}, tutto allineato", branches(heads.len()))
    };
    bus.log(tag, LogKind::Good, summary);

    Check {
        live: true,
        error: None,
        head: head.map(short),
        branches: heads.len(),
        behind,
        cloned,
    }
}

/// Point an existing clone at the project's current link. Returns true if it changed.
fn align_remote(bus: &Bus, tag: &str, dir: &Path, url: &str) -> bool {
    let current = query(dir, &["config", "--get", "remote.origin.url"]).unwrap_or_default();
    if current == url {
        return false;
    }
    let moved = !same_url(&current, url);
    let ten = Some(Duration::from_secs(10));
    if current.is_empty() {
        run(bus, tag, None, Some(dir), &["remote", "add", "origin", url], ten, false);
    } else {
        if moved {
            bus.log(tag, LogKind::Info, t!("link changed: {current} -> {url}", "cambio link: {current} -> {url}"));
        }
        run(bus, tag, None, Some(dir), &["remote", "set-url", "origin", url], ten, false);
    }
    moved
}

fn step(done: Done) -> Result<(), String> {
    if done.ok {
        Ok(())
    } else {
        Err(humanize(&done.err))
    }
}

/// Same work as the old .bat: refresh the mirror, then the working copy and its submodules.
/// With `fresh_work` the working copy is set aside (renamed, never deleted) and cloned again.
pub fn sync(bus: &Bus, p: &Project, root: &Path, fresh_work: bool) -> Result<(), String> {
    let tag = p.name.as_str();
    let id = Some(p.id);
    std::fs::create_dir_all(root).map_err(|e| t!("cannot create {}: {e}", "non riesco a creare {}: {e}", root.display()))?;

    if p.mirror {
        let dir = p.mirror_dir(root);
        if is_repo(&dir) {
            align_remote(bus, tag, &dir, &p.url);
            step(run(bus, tag, id, Some(&dir), &["fetch", "--progress", "origin"], None, true))?;
        } else {
            let target = dir.to_string_lossy().to_string();
            step(run(bus, tag, id, Some(root), &["clone", "--mirror", "--progress", &p.url, &target], None, true))?;
        }
    }

    if p.work {
        let dir = p.work_dir(root);
        if fresh_work && dir.exists() {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let aside = root.join(format!("{}.old-{stamp}", p.name));
            std::fs::rename(&dir, &aside).map_err(|e| t!("cannot move the old copy aside: {e}", "non riesco a spostare la vecchia copia: {e}"))?;
            bus.log(tag, LogKind::Info, t!("old working copy set aside in {}", "vecchia copia di lavoro messa da parte in {}", aside.display()));
        }
        if is_repo(&dir) {
            let moved = align_remote(bus, tag, &dir, &p.url);
            step(run(bus, tag, id, Some(&dir), &["pull", "--ff-only", "--progress"], None, true))?;
            if moved {
                run(bus, tag, id, Some(&dir), &["submodule", "sync", "--recursive"], None, true);
            }
            step(run(bus, tag, id, Some(&dir), &["submodule", "update", "--init", "--recursive", "--progress"], None, true))?;
        } else {
            let target = dir.to_string_lossy().to_string();
            step(run(bus, tag, id, Some(root), &["clone", "--recursive", "--progress", &p.url, &target], None, true))?;
        }
    }

    bus.log(tag, LogKind::Good, tr("synced", "sincronizzato"));
    Ok(())
}

/// One-off probe for the add/edit dialogs.
pub fn test(bus: &Bus, url: &str) {
    let done = run(bus, tr("probe", "verifica"), None, None, &["ls-remote", "--heads", url], Some(Duration::from_secs(30)), false);
    let (ok, detail) = if done.ok {
        (true, t!("answers, {}", "risponde, {}", branches(done.stdout.lines().count())))
    } else {
        (false, humanize(&done.err))
    };
    bus.send(Event::Tested {
        url: url.to_string(),
        ok,
        detail,
    });
}

/// Find repos already sitting in the backup folder (`Name-mirror.git` and `Name`).
pub fn scan(root: &Path) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let Some(folder) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if folder.contains(".old-") {
            continue;
        }
        let (name, is_mirror) = match folder.strip_suffix("-mirror.git") {
            Some(n) => (n.to_string(), true),
            None => (folder.clone(), false),
        };
        let valid = if is_mirror { dir.join("HEAD").is_file() } else { dir.join(".git").exists() };
        if !valid {
            continue;
        }
        let Some(url) = query(&dir, &["config", "--get", "remote.origin.url"]).filter(|u| !u.is_empty()) else {
            continue;
        };
        let last_sync = last_fetch(&dir, is_mirror);
        match found.iter_mut().find(|f| f.name.eq_ignore_ascii_case(&name)) {
            Some(f) => {
                f.mirror |= is_mirror;
                f.work |= !is_mirror;
                f.last_sync = f.last_sync.max(last_sync);
            }
            None => found.push(Found {
                name,
                url,
                mirror: is_mirror,
                work: !is_mirror,
                last_sync,
            }),
        }
    }
    found
}
