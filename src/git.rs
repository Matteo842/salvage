//! Everything that shells out to `git`. Runs on worker threads and reports back
//! through [`Bus`]; nothing here touches the UI state directly.

use std::collections::HashMap;
use std::io::{Read, Write};
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
    /// Files the newest releases offer, for the picker in the dialogs.
    Files {
        url: String,
        result: Result<Vec<crate::releases::Choice>, String>,
    },
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

/// `git`, or `curl` for release files. curl ships with Windows 10 and 11;
/// Git does not, so it may be missing (see [`has_git`]).
fn tool(program: &str, dir: Option<&Path>) -> Command {
    let mut c = Command::new(program);
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

/// Why `program` did not start, worded for a person.
fn spawn_error(program: &str, e: &std::io::Error) -> String {
    if e.kind() != std::io::ErrorKind::NotFound {
        t!("could not start {program}: {e}", "{program} non parte: {e}")
    } else if program == "git" {
        tr(
            "Git is not installed: get it from git-scm.com, then restart Salvage",
            "Git non è installato: scaricalo da git-scm.com, poi riavvia Salvage",
        )
        .into()
    } else {
        t!("{program} is not installed", "{program} non è installato")
    }
}

/// Git is on the PATH. Found once per run: an install made while Salvage
/// runs only shows up in a new process anyway.
fn has_git() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| tool("git", None).arg("--version").output().is_ok_and(|o| o.status.success()))
}

/// What `git ls-remote` prints: one `hash<TAB>ref` per line. Without Git, an
/// http(s) remote is asked with curl through git's own web protocol, so repos
/// kept only for their release files work on a PC with no Git at all.
fn ls_remote(bus: &Bus, tag: &str, url: &str, heads_only: bool, timeout: Duration) -> Done {
    let web = url.starts_with("https://") || url.starts_with("http://");
    if has_git() || !web {
        let mut args = vec!["ls-remote"];
        if heads_only {
            args.push("--heads");
        }
        args.push(url);
        return run(bus, tag, None, None, &args, Some(timeout), false);
    }

    let fail = |err: String| {
        bus.log(tag, LogKind::Err, err.clone());
        Done {
            ok: false,
            stdout: String::new(),
            err,
        }
    };
    let refs_url = format!("{}/info/refs?service=git-upload-pack", url.trim_end_matches('/'));
    let secs = timeout.as_secs().to_string();
    bus.log(tag, LogKind::Cmd, format!("curl {refs_url}"));
    // Raw bytes, not `run_tool`: the answer is binary-framed and holds NULs.
    let out = match tool("curl", None).args(["-sS", "-f", "-L", "--max-time", &secs, "-A", "git/2.0", &refs_url]).output() {
        Ok(o) => o,
        Err(e) => return fail(spawn_error("curl", &e)),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return fail(err.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or_default().to_string());
    }
    let Some(refs) = web_refs(&out.stdout) else {
        return fail(tr("the link does not answer like a git repository", "il link non risponde come un repository git").into());
    };
    let stdout = refs
        .iter()
        .filter(|(_, name)| !heads_only || name.starts_with("refs/heads/"))
        .map(|(hash, name)| format!("{hash}\t{name}\n"))
        .collect();
    Done {
        ok: true,
        stdout,
        err: String::new(),
    }
}

/// Refs out of an `info/refs` answer. Smart servers frame each line as
/// `<4 hex length><hash> <ref>[\0capabilities]\n`; old "dumb" ones send plain
/// `<hash>\t<ref>` lines. `None` if it is neither.
fn web_refs(body: &[u8]) -> Option<Vec<(String, String)>> {
    let is_hash = |h: &str| matches!(h.len(), 40 | 64) && h.bytes().all(|b| b.is_ascii_hexdigit());

    if body.starts_with(b"001e# service=") {
        let mut refs = Vec::new();
        let mut pos = 0;
        while pos + 4 <= body.len() {
            let len = usize::from_str_radix(std::str::from_utf8(&body[pos..pos + 4]).ok()?, 16).ok()?;
            if len == 0 {
                pos += 4; // flush
                continue;
            }
            if len < 4 || pos + len > body.len() {
                return None;
            }
            let line = &body[pos + 4..pos + len];
            pos += len;
            let line = line.split(|&b| b == 0).next().unwrap_or_default();
            let line = String::from_utf8_lossy(line);
            if let Some((hash, name)) = line.trim_end().split_once(' ') {
                // An empty repo sends one `capabilities^{}` placeholder.
                if is_hash(hash) && name != "capabilities^{}" {
                    refs.push((hash.to_string(), name.to_string()));
                }
            }
        }
        return Some(refs);
    }

    let text = String::from_utf8_lossy(body);
    let refs: Vec<_> = text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(h, _)| is_hash(h))
        .map(|(h, n)| (h.to_string(), n.trim().to_string()))
        .collect();
    (!refs.is_empty()).then_some(refs)
}

/// Quick silent query, e.g. reading a config value.
fn query(dir: &Path, args: &[&str]) -> Option<String> {
    let out = tool("git", Some(dir)).args(args).output().ok()?;
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
/// `########                  12.5%` (curl) -> ("Downloading", 0.125)
pub(crate) fn parse_progress(s: &str) -> Option<(String, f32)> {
    let head = &s[..s.find('%')?];
    let (phase, num) = match head.rfind(':') {
        Some(colon) => (head[..colon].trim().trim_start_matches("remote:").trim(), &head[colon + 1..]),
        None => {
            let (bar, num) = head.trim_end().rsplit_once(' ').unwrap_or(("", head));
            if !bar.chars().all(|c| c == '#' || c == ' ') {
                return None;
            }
            (tr("Downloading", "Download"), num)
        }
    };
    let pct: f32 = num.trim().parse().ok()?;
    Some((phase.to_string(), (pct / 100.0).clamp(0.0, 1.0)))
}

fn is_error_line(s: &str) -> bool {
    let l = s.trim_start_matches("remote:").trim_start();
    l.starts_with("fatal:") || l.starts_with("error:") || l.starts_with("Repository not found") || l.starts_with("curl:")
}

pub(crate) struct Done {
    pub ok: bool,
    pub stdout: String,
    pub err: String,
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
    run_tool(bus, tag, id, "git", dir, args, timeout, echo_stdout, None)
}

/// Run one `curl` command the same way; used for release files.
pub(crate) fn curl(bus: &Bus, tag: &str, id: Option<u64>, args: &[&str], timeout: Option<Duration>) -> Done {
    run_tool(bus, tag, id, "curl", None, args, timeout, false, None)
}

/// `curl` that also gets `input` on stdin: for secrets (`-H @-`), which must
/// not show up in the log nor in the process list.
pub(crate) fn curl_with_input(bus: &Bus, tag: &str, args: &[&str], timeout: Option<Duration>, input: &str) -> Done {
    run_tool(bus, tag, None, "curl", None, args, timeout, false, Some(input))
}

#[allow(clippy::too_many_arguments)]
fn run_tool(
    bus: &Bus,
    tag: &str,
    id: Option<u64>,
    program: &str,
    dir: Option<&Path>,
    args: &[&str],
    timeout: Option<Duration>,
    echo_stdout: bool,
    input: Option<&str>,
) -> Done {
    bus.log(tag, LogKind::Cmd, format!("{program} {}", args.join(" ")));
    let mut cmd = tool(program, dir);
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = match cmd.args(args).spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = spawn_error(program, &e);
            bus.log(tag, LogKind::Err, msg.clone());
            return Done {
                ok: false,
                stdout: String::new(),
                err: msg,
            };
        }
    };
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        // Small enough for the pipe buffer; dropping `stdin` closes it.
        let _ = stdin.write_all(text.as_bytes());
    }
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
            read_segments(err, |seg| {
                // curl writes its error right after the bar, with no line break:
                // `65.4%curl: (56) Recv failure`. Split it off or it reads as progress.
                let pieces = match seg.find("curl: (") {
                    Some(i) if i > 0 => vec![seg[..i].to_string(), seg[i..].to_string()],
                    _ => vec![seg],
                };
                for line in pieces {
                    // curl's progress bar opens with a `#=#=#` / `##O#-#` animation.
                    if line.chars().all(|c| matches!(c, '#' | '=' | 'O' | '-' | ' ')) {
                        continue;
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
                        continue;
                    }
                    let bad = is_error_line(&line);
                    if bad {
                        last_err = line.clone();
                    }
                    last_line = line.clone();
                    bus.log(&tag, if bad { LogKind::Err } else { LogKind::Out }, line);
                }
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
    // `returned error: 401/404` is curl asking a remote without Git (see `ls_remote`).
    if low.contains("repository not found")
        || low.contains("not found") && low.contains("repository")
        || low.contains("returned error: 401")
        || low.contains("returned error: 404")
    {
        tr("repository not found (removed or made private)", "repository non trovato (rimosso o reso privato)").into()
    } else if low.contains("could not resolve host") {
        tr("host unreachable (domain gone or no network)", "host non raggiungibile (dominio sparito o niente rete)").into()
    } else if low.contains("could not read username") || low.contains("authentication failed") {
        tr("server asks for a login (removed or made private)", "il server chiede un login (rimosso o reso privato)").into()
    } else if low.contains("failed to connect") || low.contains("connection timed out") || low.contains("operation timed out") || low.contains("connection refused") {
        tr("server does not answer", "il server non risponde").into()
    } else if is_transient(e) {
        t!("the connection broke {NET_TRIES} times in a row, the next sync tries again", "la connessione si è interrotta {NET_TRIES} volte di fila, la prossima sincronizzazione riprova")
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
    let cloned = is_repo(&mirror) || is_repo(&work) || (p.releases && p.releases_dir(root).is_dir());

    let done = ls_remote(bus, tag, &p.url, false, Duration::from_secs(40));
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

/// Tries for a git command that talks to the remote before giving up.
const NET_TRIES: u32 = 3;

/// The connection broke, as opposed to the remote saying no: worth another try.
/// `error processing acks: 4` is the server ending its answer halfway through.
fn is_transient(err: &str) -> bool {
    let e = err.to_lowercase();
    [
        "error processing acks",
        "rpc failed",
        "early eof",
        "unexpected disconnect",
        "the remote end hung up",
        "connection was reset",
        "connection reset",
        "recv failure",
        "send failure",
        "timed out",
        "http/2 stream",
        "returned error: 50",
        "invalid index-pack output",
        "ssl",
        "schannel",
    ]
    .iter()
    .any(|s| e.contains(s))
}

/// Run a git command that talks to the remote, again if the connection breaks.
fn run_net(bus: &Bus, tag: &str, id: Option<u64>, dir: Option<&Path>, args: &[&str]) -> Done {
    let mut attempt = 1;
    loop {
        let done = run(bus, tag, id, dir, args, None, true);
        if done.ok || attempt >= NET_TRIES || !is_transient(&done.err) || bus.cancel.load(Ordering::Relaxed) {
            return done;
        }
        attempt += 1;
        bus.log(tag, LogKind::Info, t!("the connection broke, trying again ({attempt}/{NET_TRIES})", "la connessione si è interrotta, riprovo ({attempt}/{NET_TRIES})"));
        std::thread::sleep(Duration::from_secs(3));
    }
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
            step(run_net(bus, tag, id, Some(&dir), &["fetch", "--progress", "origin"]))?;
        } else {
            let target = dir.to_string_lossy().to_string();
            step(run_net(bus, tag, id, Some(root), &["clone", "--mirror", "--progress", &p.url, &target]))?;
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
            step(run_net(bus, tag, id, Some(&dir), &["pull", "--ff-only", "--progress"]))?;
            if moved {
                run(bus, tag, id, Some(&dir), &["submodule", "sync", "--recursive"], None, true);
            }
            step(run_net(bus, tag, id, Some(&dir), &["submodule", "update", "--init", "--recursive", "--progress"]))?;
        } else {
            let target = dir.to_string_lossy().to_string();
            step(run_net(bus, tag, id, Some(root), &["clone", "--recursive", "--progress", &p.url, &target]))?;
        }
    }

    if p.releases || p.update.is_some() {
        crate::releases::sync(bus, p, root)?;
    }

    bus.log(tag, LogKind::Good, tr("synced", "sincronizzato"));
    Ok(())
}

/// One-off probe for the add/edit dialogs.
pub fn test(bus: &Bus, url: &str) {
    let done = ls_remote(bus, tr("probe", "verifica"), url, true, Duration::from_secs(30));
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

#[cfg(test)]
mod tests {
    use super::{is_transient, web_refs};

    #[test]
    fn transient() {
        assert!(is_transient("fatal: error processing acks: 4"));
        assert!(is_transient("error: RPC failed; curl 56 Recv failure: Connection was reset"));
        assert!(is_transient("fatal: early EOF"));
        assert!(!is_transient("remote: Repository not found."));
        assert!(!is_transient("fatal: Not possible to fast-forward, aborting."));
        assert!(!is_transient("fatal: could not read Username for 'https://github.com': terminal prompts disabled"));
    }

    #[test]
    fn web_refs_reads_smart_and_dumb_answers() {
        let h1 = "b".repeat(40);
        let h2 = "a".repeat(40);
        let pkt = |s: &str| format!("{:04x}{s}", s.len() + 4);
        let smart = format!(
            "{}0000{}{}0000",
            pkt("# service=git-upload-pack\n"),
            pkt(&format!("{h1} HEAD\0multi_ack symref=HEAD:refs/heads/main\n")),
            pkt(&format!("{h2} refs/heads/main\n")),
        );
        let refs = web_refs(smart.as_bytes()).unwrap();
        assert_eq!(refs, vec![(h1.clone(), "HEAD".to_string()), (h2.clone(), "refs/heads/main".to_string())]);

        let caps = format!("{} capabilities^{{}}\0agent=x\n", "0".repeat(40));
        let empty = format!("{}0000{}0000", pkt("# service=git-upload-pack\n"), pkt(&caps));
        assert_eq!(web_refs(empty.as_bytes()).unwrap(), vec![]);

        let dumb = format!("{h2}\trefs/heads/main\n");
        assert_eq!(web_refs(dumb.as_bytes()).unwrap(), vec![(h2, "refs/heads/main".to_string())]);

        assert!(web_refs(b"<!DOCTYPE html><html></html>").is_none());
    }
}
