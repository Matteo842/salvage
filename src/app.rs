//! The window: state, scheduling and all the layout.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CursorIcon, Id, Layout, Order, Painter, Rect, Sense, Stroke, StrokeKind, Ui,
    UiBuilder, ViewportCommand,
};

use crate::git::{self, Bus, Check, Event, LogKind};
use crate::headless;
use crate::lang::{branches, repos, tr};
use crate::model::{self, ago, hours_label, name_from_url, now_ts, sanitize_name, short_url, Config, Project};
use crate::paint::{self, Blip, Sig};
use crate::term::{self, Row, Term};
use crate::theme::*;
use crate::releases;
use crate::widgets::{self, button, field, toggle, Icon, Tone};

const TITLE_H: f32 = 36.0;
const GUTTER: f32 = 376.0;
const ROW_H: f32 = 64.0;
/// Repos synchronised at the same time.
const PARALLEL: usize = 2;

#[derive(Clone, Copy, PartialEq, Default)]
enum Busy {
    #[default]
    Idle,
    Queued,
    Checking,
    Syncing,
}

/// Per-repo state that lives only while the app is open.
#[derive(Default)]
struct Runtime {
    busy: Busy,
    check: Option<Check>,
    phase: String,
    pct: Option<f32>,
}

enum Act {
    SyncAll,
    Stop,
    CheckAll,
    Add,
    Edit(u64),
    Sync(u64),
    OpenDir(u64),
    OpenUrl(String),
    Settings,
    ToggleTerm,
    OpenRoot,
    Import,
    Minimize,
    ToggleMax,
    Close,
    DragWindow,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Edit(u64),
    Settings,
}

enum Probe {
    None,
    Running,
    Ok(String),
    Fail(String),
}

/// The release files a repo offers, as shown in the dialog picker. Each state
/// remembers the link it is about, so a changed link reloads the list.
enum Choices {
    None,
    Loading(String),
    Ready(String, Vec<releases::Choice>),
    Fail(String, String),
}

impl Choices {
    fn url(&self) -> Option<&str> {
        match self {
            Choices::None => None,
            Choices::Loading(u) | Choices::Ready(u, _) | Choices::Fail(u, _) => Some(u),
        }
    }
}

struct Dialog {
    kind: Kind,
    url: String,
    original_url: String,
    name: String,
    name_touched: bool,
    mirror: bool,
    work: bool,
    releases: bool,
    files: Vec<String>,
    choices: Choices,
    fresh: bool,
    old_urls: Vec<String>,
    probe: Probe,
    probe_url: String,
    error: String,
    confirm_remove: bool,
    root: String,
    hours: f32,
    auto: bool,
    task: bool,
    check_start: bool,
    focus: bool,
}

impl Dialog {
    fn blank(kind: Kind) -> Self {
        Self {
            kind,
            url: String::new(),
            original_url: String::new(),
            name: String::new(),
            name_touched: false,
            mirror: true,
            work: true,
            releases: false,
            files: Vec::new(),
            choices: Choices::None,
            fresh: false,
            old_urls: Vec::new(),
            probe: Probe::None,
            probe_url: String::new(),
            error: String::new(),
            confirm_remove: false,
            root: String::new(),
            hours: 6.0,
            auto: true,
            task: false,
            check_start: true,
            focus: true,
        }
    }
}

enum Outcome {
    None,
    Close,
    Submit,
    Remove,
    Probe,
    Browse,
    Import,
}

struct Shot {
    path: PathBuf,
    /// Seconds after launch at which to save a frame; the app quits after the last.
    at: Vec<f64>,
    next: usize,
    asked: bool,
}

pub struct App {
    cfg: Config,
    cfg_path: PathBuf,
    rx: Receiver<Event>,
    bus: Bus,
    rt: HashMap<u64, Runtime>,
    queue: VecDeque<(u64, bool)>,
    running: usize,
    checking: usize,
    term: Term,
    rows: Vec<Row>,
    term_x: f32,
    term_full: bool,
    dialog: Option<Dialog>,
    now: f64,
    shot: Option<Shot>,
    /// Tells a scheduled `--sync` that the window is open and in charge.
    _lock: Option<std::fs::File>,
}

fn hms(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn fit(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}\u{2026}")
    }
}

/// Git's progress phases, in the interface's language.
fn phase_label(phase: &str, pct: Option<f32>) -> String {
    let name = match phase {
        "Counting objects" | "Enumerating objects" => tr("counting objects", "conto gli oggetti"),
        "Compressing objects" => tr("server compressing", "il server comprime"),
        "Receiving objects" => tr("receiving objects", "ricevo gli oggetti"),
        "Resolving deltas" => tr("resolving deltas", "ricompongo i delta"),
        "Updating files" | "Checking out files" => tr("writing files", "scrivo i file"),
        "" => tr("syncing", "sincronizzo"),
        other => return other.to_lowercase(),
    };
    match pct {
        Some(p) => format!("{name} {:.0}%", p * 100.0),
        None => name.to_string(),
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply(&cc.egui_ctx);
        let cfg_path = model::config_path();
        let args: Vec<String> = std::env::args().collect();
        let arg_root = args
            .iter()
            .position(|a| a == "--root")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from);

        let loaded = Config::load(&cfg_path);
        let first_run = loaded.is_none();
        let mut cfg = loaded.unwrap_or_else(|| Config::fresh(arg_root.clone().unwrap_or_else(model::default_root)));
        let mut root_changed = false;
        if let Some(r) = arg_root {
            if cfg.root != r {
                cfg.root = r;
                root_changed = true;
            }
        }

        if cfg.auto_sync && cfg.last_sync_all.is_none() {
            cfg.last_sync_all = Some(now_ts());
        }

        let (tx, rx) = channel();
        let bus = Bus {
            tx,
            ctx: cc.egui_ctx.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let shot = std::env::var_os("SALVAGE_SHOT").map(|p| Shot {
            path: PathBuf::from(p),
            at: std::env::var("SALVAGE_SHOT_AFTER")
                .map(|s| s.split(',').filter_map(|t| t.trim().parse().ok()).collect())
                .unwrap_or_else(|_| vec![5.0]),
            next: 0,
            asked: false,
        });

        let mut app = Self {
            cfg,
            cfg_path,
            rx,
            bus,
            rt: HashMap::new(),
            queue: VecDeque::new(),
            running: 0,
            checking: 0,
            term: Term::default(),
            rows: Vec::new(),
            term_x: 20.0,
            term_full: false,
            dialog: None,
            now: 0.0,
            shot,
            _lock: model::lock(),
        };
        app.say(LogKind::Info, t!("backup folder: {}", "cartella dei backup: {}", app.cfg.root.display()));
        if first_run || root_changed {
            app.save();
            app.import();
        }
        if app.cfg.check_on_start {
            app.check_all();
        }
        match std::env::var("SALVAGE_DEMO").as_deref() {
            Ok("add") => app.open_add(),
            Ok("edit") => {
                if let Some(id) = app.cfg.projects.first().map(|p| p.id) {
                    app.open_edit(id);
                }
            }
            Ok("settings") => app.open_settings(),
            Ok("term") => app.term_full = true,
            Ok("sync") => app.sync_all(),
            _ => {}
        }
        app
    }

    fn save(&self) {
        self.cfg.save(&self.cfg_path);
    }

    fn say(&mut self, kind: LogKind, text: impl Into<String>) {
        self.term.push(self.now, "salvage", kind, text.into(), false);
    }

    fn busy(&self) -> bool {
        self.running > 0 || !self.queue.is_empty()
    }

    fn project(&self, id: u64) -> Option<&Project> {
        self.cfg.projects.iter().find(|p| p.id == id)
    }

    // ---- work scheduling -------------------------------------------------

    fn start_check(&mut self, id: u64) {
        let Some(p) = self.project(id).cloned() else {
            return;
        };
        let rt = self.rt.entry(id).or_default();
        if rt.busy != Busy::Idle {
            return;
        }
        rt.busy = Busy::Checking;
        self.checking += 1;
        let bus = self.bus.clone();
        let root = self.cfg.root.clone();
        std::thread::spawn(move || {
            let check = git::check(&bus, &p, &root);
            bus.send(Event::Checked { id, check });
        });
    }

    fn check_all(&mut self) {
        let ids: Vec<u64> = self.cfg.projects.iter().map(|p| p.id).collect();
        for id in ids {
            self.start_check(id);
        }
    }

    fn queue_sync(&mut self, id: u64, fresh: bool) {
        let rt = self.rt.entry(id).or_default();
        if matches!(rt.busy, Busy::Queued | Busy::Syncing) {
            return;
        }
        rt.busy = Busy::Queued;
        self.queue.push_back((id, fresh));
    }

    fn sync_all(&mut self) {
        let ids: Vec<u64> = self.cfg.projects.iter().map(|p| p.id).collect();
        for id in ids {
            self.queue_sync(id, false);
        }
        self.cfg.last_sync_all = Some(now_ts());
        self.save();
    }

    fn stop(&mut self) {
        self.bus.cancel.store(true, Ordering::Relaxed);
        for (id, _) in self.queue.drain(..) {
            if let Some(rt) = self.rt.get_mut(&id) {
                rt.busy = Busy::Idle;
            }
        }
        self.say(LogKind::Info, tr("stopping everything", "fermo tutto"));
    }

    fn import(&mut self) {
        self.say(
            LogKind::Info,
            t!("looking for repos already in {}", "cerco repo già presenti in {}", self.cfg.root.display()),
        );
        let bus = self.bus.clone();
        let root = self.cfg.root.clone();
        std::thread::spawn(move || {
            let found = git::scan(&root);
            bus.send(Event::Imported(found));
        });
    }

    /// Start queued work and fire the automatic run when it is due.
    fn tick(&mut self) {
        while self.running < PARALLEL {
            let Some((id, fresh)) = self.queue.pop_front() else {
                break;
            };
            let Some(p) = self.project(id).cloned() else {
                continue;
            };
            let rt = self.rt.entry(id).or_default();
            rt.busy = Busy::Syncing;
            rt.phase.clear();
            rt.pct = None;
            self.running += 1;
            let bus = self.bus.clone();
            let root = self.cfg.root.clone();
            std::thread::spawn(move || {
                let error = git::sync(&bus, &p, &root, fresh).err();
                bus.send(Event::Synced { id, error });
            });
        }
        if !self.busy() && self.checking == 0 {
            self.bus.cancel.store(false, Ordering::Relaxed);
        }
        if self.cfg.auto_sync && !self.busy() && !self.cfg.projects.is_empty() {
            if let Some(last) = self.cfg.last_sync_all {
                if now_ts() >= last + self.cfg.interval_secs() {
                    self.say(LogKind::Info, tr("time's up: syncing everything", "è ora: sincronizzo tutto"));
                    self.sync_all();
                }
            }
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Log {
                tag,
                kind,
                text,
                transient,
            } => self.term.push(self.now, &tag, kind, text, transient),
            Event::Progress { id, phase, pct } => {
                if let Some(rt) = self.rt.get_mut(&id) {
                    rt.phase = phase;
                    rt.pct = Some(pct);
                }
            }
            Event::Checked { id, check } => {
                self.checking = self.checking.saturating_sub(1);
                let rt = self.rt.entry(id).or_default();
                if rt.busy == Busy::Checking {
                    rt.busy = Busy::Idle;
                }
                rt.check = Some(check);
            }
            Event::Synced { id, error } => {
                self.running = self.running.saturating_sub(1);
                let rt = self.rt.entry(id).or_default();
                rt.busy = Busy::Idle;
                rt.phase.clear();
                rt.pct = None;
                let stopped = error.as_deref() == Some(git::stopped_msg());
                if let Some(p) = self.cfg.projects.iter_mut().find(|p| p.id == id) {
                    match &error {
                        None => {
                            p.last_sync = Some(now_ts());
                            p.last_error = None;
                        }
                        Some(e) if !stopped => p.last_error = Some(e.clone()),
                        Some(_) => {}
                    }
                }
                self.save();
                if !stopped {
                    self.start_check(id);
                }
            }
            Event::Tested { url, ok, detail } => {
                if let Some(d) = &mut self.dialog {
                    if d.probe_url == url {
                        d.probe = if ok { Probe::Ok(detail) } else { Probe::Fail(detail) };
                    }
                }
            }
            Event::Files { url, result } => {
                if let Some(d) = &mut self.dialog {
                    if matches!(&d.choices, Choices::Loading(u) if *u == url) {
                        d.choices = match result {
                            Ok(list) => Choices::Ready(url, list),
                            Err(e) => Choices::Fail(url, e),
                        };
                    }
                }
            }
            Event::Imported(found) => {
                let mut added = Vec::new();
                for f in found {
                    if !self.cfg.has_name(&f.name) {
                        let id = self.cfg.add(f.name, f.url, f.mirror, f.work);
                        if let Some(p) = self.cfg.projects.last_mut() {
                            p.last_sync = f.last_sync;
                        }
                        added.push(id);
                    }
                }
                if added.is_empty() {
                    self.say(LogKind::Info, tr("no new repos in the folder", "nessun repo nuovo nella cartella"));
                } else {
                    self.say(LogKind::Good, t!("{} repos found and added to the list", "{} repo trovati e aggiunti alla lista", added.len()));
                    self.save();
                    for id in added {
                        self.start_check(id);
                    }
                }
            }
        }
    }

    // ---- derived state ---------------------------------------------------

    /// Indicator, status wording and its colour for one repo.
    fn status(&self, p: &Project) -> (Sig, String, Color32) {
        let rt = self.rt.get(&p.id);
        match rt.map(|r| r.busy).unwrap_or_default() {
            Busy::Syncing => {
                let rt = rt.expect("syncing implies runtime");
                return (Sig::Working, phase_label(&rt.phase, rt.pct), FLARE);
            }
            Busy::Queued => return (Sig::Listening, tr("queued", "in coda").into(), KELP),
            Busy::Checking => return (Sig::Listening, tr("listening", "ascolto il link").into(), KELP),
            Busy::Idle => {}
        }
        let failed = p.last_error.as_ref().map(|e| t!("sync failed: {e}", "sincronizzazione fallita: {e}"));
        match rt.and_then(|r| r.check.as_ref()) {
            None => match failed {
                Some(f) => (Sig::Unknown, f, WRECK),
                None => (Sig::Unknown, String::new(), KELP),
            },
            Some(c) if !c.live => (
                Sig::Lost,
                t!("no answer: {}", "non risponde: {}", c.error.as_deref().unwrap_or(tr("unknown reason", "motivo sconosciuto"))),
                WRECK,
            ),
            Some(c) => {
                if let Some(f) = failed {
                    (Sig::Behind, f, WRECK)
                } else if !c.cloned {
                    (Sig::Behind, tr("to download", "da scaricare").into(), FLARE)
                } else if c.behind > 0 {
                    (Sig::Behind, t!("{} to update", "{} da aggiornare", branches(c.behind)), FLARE)
                } else {
                    (Sig::Live, tr("up to date", "allineato").into(), PING)
                }
            }
        }
    }

    /// (fraction of the interval elapsed, seconds left) when the automatic run is on.
    fn countdown(&self) -> Option<(f32, i64)> {
        if !self.cfg.auto_sync {
            return None;
        }
        let last = self.cfg.last_sync_all?;
        let every = self.cfg.interval_secs().max(1);
        let elapsed = now_ts() - last;
        Some(((elapsed as f32 / every as f32).clamp(0.0, 1.0), (every - elapsed).max(0)))
    }

    // ---- dialogs ---------------------------------------------------------

    fn open_add(&mut self) {
        self.dialog = Some(Dialog::blank(Kind::Add));
    }

    fn open_edit(&mut self, id: u64) {
        let Some(p) = self.project(id) else {
            return;
        };
        let mut d = Dialog::blank(Kind::Edit(id));
        d.url = p.url.clone();
        d.original_url = p.url.clone();
        d.name = p.name.clone();
        d.mirror = p.mirror;
        d.work = p.work;
        d.releases = p.releases;
        d.files = p.files.clone();
        d.old_urls = p.old_urls.clone();
        self.dialog = Some(d);
    }

    fn open_settings(&mut self) {
        let mut d = Dialog::blank(Kind::Settings);
        d.root = self.cfg.root.display().to_string();
        d.hours = self.cfg.interval_hours;
        d.auto = self.cfg.auto_sync;
        d.task = self.cfg.scheduled_task;
        d.check_start = self.cfg.check_on_start;
        d.focus = false;
        self.dialog = Some(d);
    }

    /// Returns true when the dialog is finished and should close.
    fn submit(&mut self, d: &mut Dialog) -> bool {
        d.error.clear();
        match d.kind {
            Kind::Add => {
                let url = d.url.trim().to_string();
                let name = sanitize_name(&d.name);
                if url.is_empty() {
                    d.error = tr("The repo link is missing.", "Manca il link del repo.").into();
                } else if name.is_empty() {
                    d.error = tr("The folder name is missing.", "Manca il nome della cartella.").into();
                } else if self.cfg.has_name(&name) {
                    d.error = t!("There is already a repo called {name}: pick another name.", "C'è già un repo chiamato {name}: scegli un altro nome.");
                } else if !d.mirror && !d.work && !d.releases {
                    d.error = tr("Pick at least one thing to keep.", "Scegli almeno una cosa da tenere.").into();
                } else if d.releases && d.files.is_empty() {
                    d.error = tr("Pick at least one release file.", "Scegli almeno un file delle release.").into();
                } else {
                    let id = self.cfg.add(name, url, d.mirror, d.work);
                    if let Some(p) = self.cfg.projects.iter_mut().find(|p| p.id == id) {
                        p.releases = d.releases;
                        p.files = d.files.clone();
                    }
                    self.save();
                    self.queue_sync(id, false);
                    return true;
                }
                false
            }
            Kind::Edit(id) => {
                let url = d.url.trim().to_string();
                if url.is_empty() {
                    d.error = tr("The repo link is missing.", "Manca il link del repo.").into();
                    return false;
                }
                if !d.mirror && !d.work && !d.releases {
                    d.error = tr("Pick at least one thing to keep.", "Scegli almeno una cosa da tenere.").into();
                    return false;
                }
                if d.releases && d.files.is_empty() {
                    d.error = tr("Pick at least one release file.", "Scegli almeno un file delle release.").into();
                    return false;
                }
                let Some(p) = self.cfg.projects.iter_mut().find(|p| p.id == id) else {
                    return true;
                };
                let moved = url != p.url;
                let changed = moved || d.fresh || d.mirror != p.mirror || d.work != p.work || d.releases != p.releases || (d.releases && d.files != p.files);
                if moved {
                    let old = std::mem::replace(&mut p.url, url.clone());
                    p.old_urls.retain(|u| *u != url && *u != old);
                    p.old_urls.insert(0, old);
                    p.old_urls.truncate(8);
                    p.last_error = None;
                    if let Some(rt) = self.rt.get_mut(&id) {
                        rt.check = None;
                    }
                }
                p.mirror = d.mirror;
                p.work = d.work;
                p.releases = d.releases;
                p.files = d.files.clone();
                self.save();
                if changed {
                    self.queue_sync(id, d.fresh);
                }
                true
            }
            Kind::Settings => {
                let root = PathBuf::from(d.root.trim());
                if root.as_os_str().is_empty() {
                    d.error = tr("The backup folder is missing.", "Manca la cartella dei backup.").into();
                    return false;
                }
                let moved = root != self.cfg.root;
                if d.task && (!self.cfg.scheduled_task || d.hours != self.cfg.interval_hours) {
                    if let Err(e) = headless::schedule(d.hours) {
                        d.error = fit(&t!("Task Scheduler: {e}", "Utilità di pianificazione: {e}"), 72);
                        return false;
                    }
                    self.say(LogKind::Good, t!("Windows scheduled task set: every {}", "attività pianificata in Windows: ogni {}", hours_label(d.hours)));
                } else if !d.task && self.cfg.scheduled_task {
                    match headless::unschedule() {
                        Ok(()) => self.say(LogKind::Info, tr("Windows scheduled task removed", "attività pianificata rimossa da Windows")),
                        Err(e) => self.say(LogKind::Err, t!("scheduled task not removed: {e}", "attività pianificata non rimossa: {e}")),
                    }
                }
                self.cfg.scheduled_task = d.task;
                self.cfg.root = root;
                self.cfg.interval_hours = d.hours;
                if d.auto && (!self.cfg.auto_sync || self.cfg.last_sync_all.is_none()) {
                    self.cfg.last_sync_all = Some(now_ts());
                }
                self.cfg.auto_sync = d.auto;
                self.cfg.check_on_start = d.check_start;
                self.save();
                if moved {
                    for rt in self.rt.values_mut() {
                        rt.check = None;
                    }
                    self.import();
                    self.check_all();
                }
                true
            }
        }
    }

    fn dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.dialog.take() else {
            return;
        };
        let url = d.url.trim().to_string();
        let typing = ctx.memory(|m| m.has_focus(Id::new("dlg_url")));
        if d.releases && !url.is_empty() && !typing && d.choices.url() != Some(url.as_str()) {
            d.choices = Choices::Loading(url.clone());
            let bus = self.bus.clone();
            std::thread::spawn(move || releases::list_files(&bus, &url));
        }
        let mut out = Outcome::None;
        let screen = ctx.screen_rect();
        let body = Rect::from_min_max(pos2(screen.left(), screen.top() + TITLE_H + 1.0), screen.max);
        let moved = d.kind != Kind::Add && d.url.trim() != d.original_url;
        let height = match d.kind {
            Kind::Add => 336.0 + releases_h(&d),
            Kind::Edit(_) => 384.0 + releases_h(&d) + if d.old_urls.is_empty() { 0.0 } else { 26.0 + 20.0 * d.old_urls.len().min(4) as f32 },
            Kind::Settings => 404.0,
        };

        egui::Area::new(Id::new("dialog"))
            .order(Order::Foreground)
            .fixed_pos(body.min)
            .show(ctx, |ui| {
                let scrim = ui.allocate_rect(body, Sense::click());
                let p = ui.painter().with_clip_rect(body);
                p.rect_filled(body, 0, alpha(ABYSS, 0.72));
                let rect = Rect::from_center_size(body.center(), vec2(580.0, height));
                paint::glass(&p, rect, &self.rows, self.term_x);
                if scrim.clicked() && !ctx.pointer_interact_pos().is_some_and(|q| rect.contains(q)) {
                    out = Outcome::Close;
                }
                let inner = rect.shrink2(vec2(30.0, 26.0));
                let mut ui = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
                ui.set_clip_rect(rect);
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                let w = inner.width();
                let note = |ui: &mut Ui, text: &str, col: Color32| {
                    ui.label(egui::RichText::new(text).font(mono(11.5)).color(col));
                };

                match d.kind {
                    Kind::Add => {
                        ui.label(egui::RichText::new(tr("Add a repo", "Aggiungi un repo")).font(mono(16.0)).color(FOAM));
                        note(&mut ui, tr("Paste the git link. GitHub, GitLab, Gitea or any other host works.", "Incolla il link git. Va bene GitHub, GitLab, Gitea o qualsiasi altro host."), KELP);
                        ui.add_space(6.0);
                        let r = field(&mut ui, Id::new("dlg_url"), &mut d.url, tr("https://host/user/repo", "https://host/utente/repo"), w);
                        if d.focus {
                            r.request_focus();
                            d.focus = false;
                        }
                        if r.changed() {
                            if !d.name_touched {
                                d.name = name_from_url(&d.url);
                            }
                            d.probe = Probe::None;
                        }
                        if field(&mut ui, Id::new("dlg_name"), &mut d.name, tr("folder name", "nome della cartella"), w).changed() {
                            d.name_touched = true;
                        }
                        ui.add_space(2.0);
                        toggle(&mut ui, &mut d.mirror, tr("full mirror: every branch and tag", "mirror completo: tutti i branch e i tag"));
                        toggle(&mut ui, &mut d.work, tr("working copy: files ready to open, with submodules", "copia di lavoro: i file pronti da aprire, con i submodule"));
                        releases_ui(&mut ui, &mut d, w);
                    }
                    Kind::Edit(_) => {
                        ui.label(egui::RichText::new(&d.name).font(mono(16.0)).color(FOAM));
                        note(&mut ui, tr("If the repo moved, paste the new link here. The folders on disk stay the same.", "Se il repo si è spostato, incolla qui il link nuovo. Le cartelle su disco restano le stesse."), KELP);
                        ui.add_space(6.0);
                        if field(&mut ui, Id::new("dlg_url"), &mut d.url, tr("https://host/user/repo", "https://host/utente/repo"), w).changed() {
                            d.probe = Probe::None;
                        }
                        if !d.old_urls.is_empty() {
                            note(&mut ui, tr("links used before (click to restore)", "link usati prima (clicca per rimetterlo)"), DEEP);
                            for (i, old) in d.old_urls.clone().iter().take(4).enumerate() {
                                let (r, _) = ui.allocate_exact_size(vec2(w, 12.0), Sense::hover());
                                let text = fit(&short_url(old), 68);
                                if widgets::link(&mut ui, Id::new(("old_url", i)), r.min, text, 11.5, KELP).clicked() {
                                    d.url = old.clone();
                                    d.probe = Probe::None;
                                }
                            }
                        }
                        ui.add_space(2.0);
                        toggle(&mut ui, &mut d.mirror, tr("full mirror: every branch and tag", "mirror completo: tutti i branch e i tag"));
                        toggle(&mut ui, &mut d.work, tr("working copy: files ready to open, with submodules", "copia di lavoro: i file pronti da aprire, con i submodule"));
                        releases_ui(&mut ui, &mut d, w);
                        if d.work {
                            toggle(&mut ui, &mut d.fresh, tr("re-clone the working copy from scratch (the old one stays as .old)", "riscarica la copia di lavoro da zero (la vecchia resta come .old)"));
                        }
                        if moved {
                            note(&mut ui, tr("The current link moves to the history. Mirror and working copy will follow the new one.", "Il link attuale passa nello storico. Mirror e copia di lavoro seguiranno quello nuovo."), FLARE);
                        }
                    }
                    Kind::Settings => {
                        ui.label(egui::RichText::new(tr("Settings", "Impostazioni")).font(mono(16.0)).color(FOAM));
                        note(&mut ui, tr("Folder where mirrors and working copies live.", "Cartella dove vivono mirror e copie di lavoro."), KELP);
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            field(ui, Id::new("dlg_root"), &mut d.root, "D:\\backup", w - 108.0);
                            if button(ui, Some(Icon::Folder), tr("browse", "sfoglia"), Tone::Quiet).clicked() {
                                out = Outcome::Browse;
                            }
                        });
                        if button(&mut ui, Some(Icon::Scan), tr("find repos already in this folder", "cerca repo già presenti in questa cartella"), Tone::Quiet).clicked() {
                            out = Outcome::Import;
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            toggle(ui, &mut d.auto, tr("sync everything automatically, every", "sincronizza tutto da solo, ogni"));
                            widgets::interval_stepper(ui, &mut d.hours);
                        });
                        toggle(&mut ui, &mut d.task, tr("also while the app is closed, via Windows Task Scheduler", "anche ad app chiusa, con l'Utilità di pianificazione di Windows"));
                        toggle(&mut ui, &mut d.check_start, tr("check the links at startup", "controlla i link all'avvio"));
                        note(&mut ui, tr("While closed, Windows starts Salvage with no window, syncs and quits: nothing stays running.", "Ad app chiusa Windows avvia Salvage senza finestra, sincronizza e lo richiude: niente resta acceso."), DEEP);
                    }
                }

                // Probe result and validation errors sit just above the buttons.
                let line_y = inner.bottom() - 58.0;
                if !d.error.is_empty() {
                    ui.painter()
                        .text(pos2(inner.left(), line_y), Align2::LEFT_TOP, &d.error, mono(11.5), WRECK);
                } else {
                    let (sig, text, col) = match &d.probe {
                        Probe::None => (Sig::Unknown, String::new(), KELP),
                        Probe::Running => (Sig::Listening, tr("listening", "ascolto il link").to_string(), KELP),
                        Probe::Ok(t) => (Sig::Live, fit(t, 66), PING),
                        Probe::Fail(t) => (Sig::Lost, fit(&t!("no answer: {t}", "non risponde: {t}"), 66), WRECK),
                    };
                    if !text.is_empty() {
                        paint::ping(ui.painter(), pos2(inner.left() + 9.0, line_y + 8.0), sig, self.now, 3);
                        ui.painter()
                            .text(pos2(inner.left() + 28.0, line_y), Align2::LEFT_TOP, text, mono(11.5), col);
                    }
                }

                let bar = Rect::from_min_max(pos2(inner.left(), inner.bottom() - 30.0), inner.max);
                ui.allocate_new_ui(
                    UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)),
                    |ui| {
                        let main = match d.kind {
                            Kind::Add => tr("add and download", "aggiungi e scarica"),
                            Kind::Edit(_) if moved || d.fresh => tr("save and sync", "salva e sincronizza"),
                            _ => tr("save", "salva"),
                        };
                        if button(ui, None, main, Tone::Primary).clicked() {
                            out = Outcome::Submit;
                        }
                        if button(ui, None, tr("cancel", "annulla"), Tone::Quiet).clicked() {
                            out = Outcome::Close;
                        }
                        if d.kind != Kind::Settings && button(ui, Some(Icon::Scan), tr("test link", "prova il link"), Tone::Quiet).clicked() {
                            out = Outcome::Probe;
                        }
                    },
                );
                if matches!(d.kind, Kind::Edit(_)) {
                    // Top corner, away from the save buttons: the bar has no room for it
                    // and it should not sit next to the main action anyway.
                    let corner = Rect::from_min_max(pos2(inner.right() - 260.0, inner.top() - 8.0), pos2(inner.right(), inner.top() + 22.0));
                    ui.allocate_new_ui(
                        UiBuilder::new().max_rect(corner).layout(Layout::right_to_left(Align::Center)),
                        |ui| {
                            let label = if d.confirm_remove { tr("confirm: files stay", "conferma: i file restano") } else { tr("remove from list", "togli dalla lista") };
                            if button(ui, None, label, Tone::Danger).clicked() {
                                if d.confirm_remove {
                                    out = Outcome::Remove;
                                } else {
                                    d.confirm_remove = true;
                                }
                            }
                        },
                    );
                }
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            out = Outcome::Close;
        }
        if matches!(out, Outcome::None) && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            out = Outcome::Submit;
        }

        match out {
            Outcome::None => self.dialog = Some(d),
            Outcome::Close => {}
            Outcome::Submit => {
                if !self.submit(&mut d) {
                    self.dialog = Some(d);
                }
            }
            Outcome::Remove => {
                if let Kind::Edit(id) = d.kind {
                    if let Some(p) = self.project(id) {
                        let name = p.name.clone();
                        self.say(LogKind::Info, t!("{name} removed from the list, files stay on disk", "{name} tolto dalla lista, i file restano su disco"));
                    }
                    self.cfg.projects.retain(|p| p.id != id);
                    self.queue.retain(|(q, _)| *q != id);
                    self.rt.remove(&id);
                    self.save();
                }
            }
            Outcome::Probe => {
                let url = d.url.trim().to_string();
                if url.is_empty() {
                    d.error = tr("The repo link is missing.", "Manca il link del repo.").into();
                } else {
                    d.error.clear();
                    d.probe = Probe::Running;
                    d.probe_url = url.clone();
                    let bus = self.bus.clone();
                    std::thread::spawn(move || git::test(&bus, &url));
                }
                self.dialog = Some(d);
            }
            Outcome::Browse => {
                if let Some(dir) = rfd::FileDialog::new().set_directory(d.root.trim()).pick_folder() {
                    d.root = dir.display().to_string();
                }
                self.dialog = Some(d);
            }
            Outcome::Import => {
                if self.submit(&mut d) {
                    self.import();
                } else {
                    self.dialog = Some(d);
                }
            }
        }
    }

    // ---- main surface ----------------------------------------------------

    fn title_bar(&self, ui: &mut Ui, p: &Painter, rect: Rect, acts: &mut Vec<Act>) {
        p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, LINE));
        let drag = ui.interact(rect, Id::new("title_drag"), Sense::click_and_drag());
        if drag.double_clicked() {
            acts.push(Act::ToggleMax);
        } else if drag.drag_started_by(egui::PointerButton::Primary) {
            acts.push(Act::DragWindow);
        }
        let cy = rect.center().y;
        let mark = pos2(rect.left() + 22.0, cy);
        p.circle_stroke(mark, 6.0, Stroke::new(1.2, PING));
        p.circle_filled(mark, 1.8, PING);
        let name = p.text(pos2(rect.left() + 38.0, cy), Align2::LEFT_CENTER, "salvage", mono(12.5), FOAM);
        let root = self.cfg.root.display().to_string();
        if widgets::link(ui, Id::new("root_link"), pos2(name.right() + 18.0, cy - 7.0), root, 11.5, DEEP)
            .on_hover_text(tr("open the backup folder", "apri la cartella dei backup"))
            .clicked()
        {
            acts.push(Act::OpenRoot);
        }

        let mut x = rect.right() - 6.0;
        let mut slot = |w: f32| {
            x -= w;
            Rect::from_min_size(pos2(x, rect.top() + 4.0), vec2(w, 28.0))
        };
        if widgets::icon_button_at(ui, slot(34.0), Id::new("w_close"), Icon::Close, tr("close", "chiudi"), WRECK).clicked() {
            acts.push(Act::Close);
        }
        if widgets::icon_button_at(ui, slot(34.0), Id::new("w_max"), Icon::Max, tr("maximize", "ingrandisci"), FOAM).clicked() {
            acts.push(Act::ToggleMax);
        }
        if widgets::icon_button_at(ui, slot(34.0), Id::new("w_min"), Icon::Min, tr("minimize", "riduci a icona"), FOAM).clicked() {
            acts.push(Act::Minimize);
        }
        slot(14.0);
        if widgets::icon_button_at(ui, slot(34.0), Id::new("w_set"), Icon::Sliders, tr("settings", "impostazioni"), FOAM).clicked() {
            acts.push(Act::Settings);
        }
        let tip = if self.term_full { tr("back to the list (F2)", "torna alla lista (F2)") } else { tr("full-screen terminal (F2)", "terminale a tutto schermo (F2)") };
        let term_slot = slot(34.0);
        if self.term_full {
            p.line_segment(
                [pos2(term_slot.left() + 8.0, rect.bottom() - 1.0), pos2(term_slot.right() - 8.0, rect.bottom() - 1.0)],
                Stroke::new(2.0, PING),
            );
        }
        if widgets::icon_button_at(ui, term_slot, Id::new("w_term"), Icon::Term, tip, PING).clicked() {
            acts.push(Act::ToggleTerm);
        }
    }

    /// Wordmark, scope and countdown. Returns the y where the terminal may start.
    fn gutter(&self, ui: &mut Ui, p: &Painter, left: f32, top: f32, acts: &mut Vec<Act>) -> f32 {
        let x = left + 20.0;
        let mut y = paint::logo(p, pos2(x, top + 20.0), GUTTER - 48.0) + 10.0;
        p.text(
            pos2(x, y),
            Align2::LEFT_TOP,
            tr("keeps the repos that might vanish safe", "tiene in salvo i repo che potrebbero sparire"),
            mono(10.5),
            KELP,
        );
        y += 32.0;

        let r = 80.0;
        let c = pos2(left + GUTTER / 2.0 - 6.0, y + r + 10.0);
        let blips: Vec<Blip> = self
            .cfg
            .projects
            .iter()
            .map(|pr| Blip {
                seed: pr.id,
                name: pr.name.clone(),
                sig: self.status(pr).0,
            })
            .collect();
        let cd = self.countdown();
        paint::scope(p, c, r, self.now, &blips, cd.map(|c| c.0));
        y = c.y + r + 26.0;

        let (first, second) = if self.busy() {
            let left = self.queue.len() + self.running;
            (tr("sync in progress", "sincronizzazione in corso").to_string(), t!("{} still to go", "{} ancora da finire", repos(left)))
        } else {
            match cd {
                Some((_, secs)) => (
                    t!("next sync in {}", "prossima sincronizzazione tra {}", hms(secs)),
                    t!(
                        "every {}, {}",
                        "ogni {}, {}",
                        hours_label(self.cfg.interval_hours),
                        if self.cfg.scheduled_task { tr("even when closed", "anche ad app chiusa") } else { tr("only while open", "solo ad app aperta") }
                    ),
                ),
                None => (
                    tr("automatic sync is off", "sincronizzazione automatica spenta").to_string(),
                    tr("click to turn it on", "clicca per riaccenderla").to_string(),
                ),
            }
        };
        let hit = Rect::from_center_size(pos2(c.x, y + 17.0), vec2(GUTTER - 50.0, 40.0));
        let resp = ui
            .interact(hit, Id::new("countdown"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        let col = if resp.hovered() { FOAM } else { mix(KELP, FOAM, 0.55) };
        p.text(pos2(c.x, y), Align2::CENTER_TOP, first, mono(12.0), col);
        p.text(pos2(c.x, y + 19.0), Align2::CENTER_TOP, second, mono(10.5), DEEP);
        if resp.clicked() {
            acts.push(Act::Settings);
        }
        y + 50.0
    }

    fn sheet_ui(&self, ui: &mut Ui, rect: Rect, acts: &mut Vec<Act>) {
        let mut ui = ui.new_child(UiBuilder::new().max_rect(rect).id_salt("sheet"));
        ui.set_clip_rect(rect.shrink(1.0).intersect(ui.clip_rect()));
        let head = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 60.0));

        let bar = Rect::from_min_max(pos2(head.left() + 120.0, head.top()), pos2(head.right() - 18.0, head.bottom()));
        let buttons = ui
            .allocate_new_ui(
                UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)),
                |ui| {
                    if self.busy() {
                        if button(ui, Some(Icon::Stop), tr("stop", "ferma"), Tone::Danger).clicked() {
                            acts.push(Act::Stop);
                        }
                    } else if button(ui, Some(Icon::Sync), tr("sync all", "sincronizza tutto"), Tone::Primary).clicked() {
                        acts.push(Act::SyncAll);
                    }
                    if button(ui, Some(Icon::Scan), tr("check", "controlla"), Tone::Quiet)
                        .on_hover_text(tr("asks every link whether it still answers", "chiede a ogni link se risponde ancora"))
                        .clicked()
                    {
                        acts.push(Act::CheckAll);
                    }
                    if button(ui, Some(Icon::Plus), tr("add", "aggiungi"), Tone::Quiet).clicked() {
                        acts.push(Act::Add);
                    }
                    ui.min_rect()
                },
            )
            .inner;

        // Tally, clipped so it never runs under the buttons.
        let (mut live, mut lost, mut todo) = (0, 0, 0);
        for pr in &self.cfg.projects {
            match self.status(pr).0 {
                Sig::Live => live += 1,
                Sig::Lost => lost += 1,
                Sig::Behind => todo += 1,
                _ => {}
            }
        }
        let tally = ui
            .painter()
            .with_clip_rect(Rect::from_min_max(head.min, pos2(buttons.left() - 14.0, head.bottom())));
        let mut x = head.left() + 26.0;
        let cy = head.center().y;
        let put = |x: &mut f32, text: String, col: Color32| {
            let r = tally.text(pos2(*x, cy), Align2::LEFT_CENTER, text, mono(12.5), col);
            *x = r.right() + 22.0;
        };
        // Symbol + count; the words live in the tooltip.
        let mark = |ui: &mut Ui, x: &mut f32, ic: Icon, n: usize, col: Color32, key: &str, tip: String| {
            let from = *x;
            widgets::icon(&tally, pos2(from + 7.0, cy), 6.5, ic, col);
            let r = tally.text(pos2(from + 19.0, cy), Align2::LEFT_CENTER, n.to_string(), mono(12.5), col);
            let hit = Rect::from_min_max(pos2(from - 4.0, cy - 14.0), pos2(r.right() + 4.0, cy + 14.0));
            ui.interact(hit, Id::new(("tally", key)), Sense::hover()).on_hover_text(tip);
            *x = r.right() + 22.0;
        };
        put(&mut x, repos(self.cfg.projects.len()), FOAM);
        if live > 0 {
            put(&mut x, format!("{live} online"), PING);
        }
        if todo > 0 {
            mark(&mut ui, &mut x, Icon::Down, todo, FLARE, "todo", t!("{todo} to update", "{todo} da aggiornare"));
        }
        if lost > 0 {
            let tip = if lost == 1 { tr("1 does not answer", "1 non risponde").to_string() } else { t!("{lost} do not answer", "{lost} non rispondono") };
            mark(&mut ui, &mut x, Icon::Skull, lost, WRECK, "lost", tip);
        }
        ui.painter()
            .line_segment([head.left_bottom(), head.right_bottom()], Stroke::new(1.0, LINE));

        let list = Rect::from_min_max(pos2(rect.left() + 1.0, head.bottom() + 1.0), rect.max - vec2(1.0, 1.0));
        let mut lui = ui.new_child(UiBuilder::new().max_rect(list).id_salt("list"));
        lui.set_clip_rect(list.intersect(ui.clip_rect()));
        if self.cfg.projects.is_empty() {
            self.empty_state(&mut lui, list, acts);
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut lui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            for pr in &self.cfg.projects {
                self.row(ui, pr, acts);
            }
        });
    }

    fn empty_state(&self, ui: &mut Ui, area: Rect, acts: &mut Vec<Act>) {
        let c = area.center() - vec2(0.0, 40.0);
        ui.painter()
            .text(c, Align2::CENTER_CENTER, tr("The list is empty.", "La lista è vuota."), mono(15.0), FOAM);
        ui.painter().text(
            c + vec2(0.0, 26.0),
            Align2::CENTER_CENTER,
            tr("Add a git link, or look for repos already in the folder.", "Aggiungi un link git, oppure cerca i repo già presenti nella cartella."),
            mono(11.5),
            KELP,
        );
        let bar = Rect::from_center_size(c + vec2(0.0, 70.0), vec2(360.0, 32.0));
        ui.allocate_new_ui(
            UiBuilder::new().max_rect(bar).layout(Layout::left_to_right(Align::Center)),
            |ui| {
                if button(ui, Some(Icon::Plus), tr("add a repo", "aggiungi un repo"), Tone::Primary).clicked() {
                    acts.push(Act::Add);
                }
                if button(ui, Some(Icon::Scan), tr("search the folder", "cerca nella cartella"), Tone::Quiet).clicked() {
                    acts.push(Act::Import);
                }
            },
        );
    }

    fn row(&self, ui: &mut Ui, pr: &Project, acts: &mut Vec<Act>) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let p = ui.painter().clone();
        if ui.rect_contains_pointer(rect) {
            p.rect_filled(rect, 0, alpha(FOAM, 0.028));
        }
        let (sig, word, word_col) = self.status(pr);
        let rt = self.rt.get(&pr.id);
        paint::ping(&p, pos2(rect.left() + 30.0, rect.center().y), sig, self.now, pr.id);

        let x = rect.left() + 58.0;
        let actions_left = rect.right() - 18.0 - 3.0 * 32.0;
        let text_right = actions_left - 16.0;

        let name = p.text(pos2(x, rect.top() + 12.0), Align2::LEFT_TOP, &pr.name, mono(14.5), FOAM);
        let room = ((text_right - name.right() - 14.0) / 7.0).max(0.0) as usize;
        p.text(
            pos2(name.right() + 14.0, rect.top() + 15.5),
            Align2::LEFT_TOP,
            fit(&word, room),
            mono(11.5),
            word_col,
        );

        // Second line: where it lives, then what we know about it.
        let mut meta = match pr.last_sync {
            Some(ts) => t!("synced {}", "sincronizzato {}", ago(ts, now_ts())),
            None => tr("never synced", "mai sincronizzato").to_string(),
        };
        if let Some(c) = rt.and_then(|r| r.check.as_ref()).filter(|c| c.live) {
            meta.push_str(&format!("   {}", branches(c.branches)));
            if let Some(h) = &c.head {
                meta.push_str(&format!("   {h}"));
            }
        }
        let meta_rect = p.text(pos2(text_right, rect.top() + 38.0), Align2::RIGHT_TOP, meta, mono(11.0), DEEP);
        let room = ((meta_rect.left() - 18.0 - x) / 6.95).max(8.0) as usize;
        let shown = fit(&short_url(&pr.url), room);
        let url = widgets::link(ui, Id::new(("row_url", pr.id)), pos2(x, rect.top() + 37.5), shown, 11.5, KELP);
        if url.clicked() {
            if let Some(web) = model::web_url(&pr.url) {
                acts.push(Act::OpenUrl(web));
            }
        }

        let slot = |i: f32| Rect::from_min_size(pos2(actions_left + i * 32.0, rect.center().y - 15.0), vec2(30.0, 30.0));
        if widgets::icon_button_at(ui, slot(0.0), Id::new(("row_sync", pr.id)), Icon::Sync, tr("sync now", "sincronizza adesso"), FLARE).clicked() {
            acts.push(Act::Sync(pr.id));
        }
        if widgets::icon_button_at(ui, slot(1.0), Id::new(("row_edit", pr.id)), Icon::Edit, tr("edit or change link", "modifica o cambia link"), FOAM).clicked() {
            acts.push(Act::Edit(pr.id));
        }
        if widgets::icon_button_at(ui, slot(2.0), Id::new(("row_dir", pr.id)), Icon::Folder, tr("open folder", "apri la cartella"), FOAM).clicked() {
            acts.push(Act::OpenDir(pr.id));
        }

        let end = rect.right() - 18.0;
        p.line_segment(
            [pos2(x, rect.bottom() - 0.5), pos2(end, rect.bottom() - 0.5)],
            Stroke::new(1.0, alpha(LINE, 0.7)),
        );
        if sig == Sig::Working {
            let y = rect.bottom() - 1.0;
            let (a, b) = match rt.and_then(|r| r.pct) {
                Some(f) => (x, x + (end - x) * f),
                None => {
                    let t = (self.now as f32 * 0.6).fract();
                    let w = (end - x) * 0.22;
                    let s = x + (end - x + w) * t - w;
                    (s.max(x), (s + w).min(end))
                }
            };
            p.line_segment([pos2(a, y), pos2(b, y)], Stroke::new(2.0, FLARE));
        }
    }

    /// Paints everything below the dialogs. Returns true when the pointer is over open terminal.
    fn body(&self, ui: &mut Ui, acts: &mut Vec<Act>) -> bool {
        let ctx = ui.ctx().clone();
        let screen = ui.max_rect();
        let p = ui.painter().clone();
        let title = Rect::from_min_max(screen.min, pos2(screen.right(), screen.top() + TITLE_H));
        let t = ctx.animate_bool_with_time(Id::new("term_full"), self.term_full, 0.32);
        let ease = t * t * (3.0 - 2.0 * t);
        let home = Rect::from_min_max(
            pos2(screen.left() + GUTTER, title.bottom() + 8.0),
            pos2(screen.right() - 14.0, screen.bottom() - 14.0),
        );
        let sheet = home.translate(vec2(ease * (home.width() + 40.0), 0.0));

        self.title_bar(ui, &p, title, acts);
        let gutter_bottom = self.gutter(ui, &p, screen.left(), title.bottom(), acts);

        let split = screen.left() + GUTTER - 10.0;
        let below = Rect::from_min_max(pos2(screen.left(), gutter_bottom), pos2(split, screen.bottom() - 1.0));
        term::paint_sharp(&p, &self.rows, self.term_x, below, 0.95, 44.0);
        let beside = Rect::from_min_max(
            pos2(split, title.bottom() + 1.0),
            pos2((sheet.left() - 10.0).min(screen.right()), screen.bottom() - 1.0),
        );
        term::paint_sharp(&p, &self.rows, self.term_x, beside, 0.95, 0.0);

        if sheet.left() < screen.right() {
            paint::glass(&p, sheet, &self.rows, self.term_x);
            self.sheet_ui(ui, sheet, acts);
        }

        if !ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            p.rect_stroke(screen, 0, Stroke::new(1.0, LINE), StrokeKind::Inside);
        }
        ctx.pointer_hover_pos()
            .is_some_and(|q| below.contains(q) || (beside.width() > 0.0 && beside.contains(q)))
    }

    fn apply(&mut self, ctx: &egui::Context, act: Act) {
        match act {
            Act::SyncAll => self.sync_all(),
            Act::Stop => self.stop(),
            Act::CheckAll => self.check_all(),
            Act::Add => self.open_add(),
            Act::Edit(id) => self.open_edit(id),
            Act::Sync(id) => self.queue_sync(id, false),
            Act::Settings => self.open_settings(),
            Act::ToggleTerm => self.term_full = !self.term_full,
            Act::Import => self.import(),
            Act::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Act::OpenRoot => open_folder(&self.cfg.root),
            Act::OpenDir(id) => {
                if let Some(p) = self.project(id) {
                    let work = p.work_dir(&self.cfg.root);
                    let mirror = p.mirror_dir(&self.cfg.root);
                    let releases = p.releases_dir(&self.cfg.root);
                    if work.exists() {
                        open_folder(&work);
                    } else if mirror.exists() {
                        open_folder(&mirror);
                    } else if releases.exists() {
                        open_folder(&releases);
                    } else {
                        open_folder(&self.cfg.root);
                    }
                }
            }
            Act::Minimize => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
            Act::ToggleMax => {
                let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
            }
            Act::Close => ctx.send_viewport_cmd(ViewportCommand::Close),
            Act::DragWindow => ctx.send_viewport_cmd(ViewportCommand::StartDrag),
        }
    }

    /// The window has no OS frame, so the edges have to offer resizing themselves.
    fn resize_edges(&self, ctx: &egui::Context) {
        use egui::ResizeDirection as D;
        if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
            return;
        }
        let Some(q) = ctx.pointer_hover_pos() else {
            return;
        };
        let s = ctx.screen_rect();
        let m = 5.0;
        let (l, r) = (q.x < s.left() + m, q.x > s.right() - m);
        let (t, b) = (q.y < s.top() + m, q.y > s.bottom() - m);
        let hit = match (l, r, t, b) {
            (true, _, true, _) => Some((D::NorthWest, CursorIcon::ResizeNwSe)),
            (_, true, true, _) => Some((D::NorthEast, CursorIcon::ResizeNeSw)),
            (true, _, _, true) => Some((D::SouthWest, CursorIcon::ResizeNeSw)),
            (_, true, _, true) => Some((D::SouthEast, CursorIcon::ResizeNwSe)),
            (true, ..) => Some((D::West, CursorIcon::ResizeHorizontal)),
            (_, true, ..) => Some((D::East, CursorIcon::ResizeHorizontal)),
            (_, _, true, _) => Some((D::North, CursorIcon::ResizeVertical)),
            (_, _, _, true) => Some((D::South, CursorIcon::ResizeVertical)),
            _ => None,
        };
        if let Some((dir, cursor)) = hit {
            ctx.set_cursor_icon(cursor);
            if ctx.input(|i| i.pointer.primary_pressed()) {
                ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
            }
        }
    }

    /// Developer aid: `SALVAGE_SHOT=file.png` saves one frame and quits.
    fn shot_tick(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.shot else {
            return;
        };
        let Some(&due) = s.at.get(s.next) else {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        };
        if !s.asked && self.now >= due {
            s.asked = true;
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(Default::default()));
        }
        let img = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = img {
            let path = if s.next == 0 {
                s.path.clone()
            } else {
                s.path.with_extension(format!("{}.png", s.next))
            };
            s.next += 1;
            s.asked = false;
            let _ = image::save_buffer(
                &path,
                img.as_raw(),
                img.width() as u32,
                img.height() as u32,
                image::ColorType::Rgba8,
            );
        }
    }
}

fn open_folder(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [5.0 / 255.0, 13.0 / 255.0, 20.0 / 255.0, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.now = ctx.input(|i| i.time);
        while let Ok(ev) = self.rx.try_recv() {
            self.handle(ev);
        }
        self.tick();

        let screen = ctx.screen_rect();
        let area = Rect::from_min_max(pos2(screen.left(), screen.top() + TITLE_H), screen.max);
        self.term_x = screen.left() + 20.0;
        self.term.clamp_scroll(area.height() / term::ROW_H - 2.0);
        self.rows = self.term.rows(ctx, area, self.now);

        let mut acts = Vec::new();
        let over_term = egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(ABYSS))
            .show(ctx, |ui| self.body(ui, &mut acts))
            .inner;

        let modal = self.dialog.is_some();
        if over_term && !modal {
            let dy = ctx.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.term.scroll = (self.term.scroll + dy / term::ROW_H).max(0.0);
            }
        }
        if !modal && ctx.input(|i| i.key_pressed(egui::Key::F2)) {
            acts.push(Act::ToggleTerm);
        }

        self.dialog_ui(ctx);
        for act in acts {
            self.apply(ctx, act);
        }
        self.resize_edges(ctx);
        self.shot_tick(ctx);

        // The scope keeps sweeping, so keep painting: fast while something is
        // happening, slower when idle, slowest when the window is not in front.
        if self.busy() || self.checking > 0 || self.term.typing(self.now) {
            ctx.request_repaint();
        } else if ctx.input(|i| i.focused) {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

const PICK_ROWS: usize = 6;

/// Dialog height taken by the release toggle and, when on, its picker.
fn releases_h(d: &Dialog) -> f32 {
    if !d.releases {
        return 32.0;
    }
    let list = match &d.choices {
        Choices::Ready(_, list) => 30.0 * list.len().clamp(1, PICK_ROWS) as f32,
        _ => 22.0,
    };
    32.0 + list + 8.0 + 48.0
}

/// Release files toggle and, when on, one switch per file the newest release has.
fn releases_ui(ui: &mut Ui, d: &mut Dialog, w: f32) {
    toggle(ui, &mut d.releases, tr("release files: builds and installers", "file delle release: build e installer"));
    if !d.releases {
        return;
    }
    let note = |ui: &mut Ui, text: &str, col: Color32| {
        ui.label(egui::RichText::new(text).font(mono(11.5)).color(col));
    };
    match &d.choices {
        Choices::None => note(ui, tr("paste the link to see the files", "incolla il link per vedere i file"), KELP),
        Choices::Loading(_) => note(ui, tr("reading the releases...", "leggo le release..."), KELP),
        Choices::Fail(_, e) => note(ui, &fit(e, 72), WRECK),
        Choices::Ready(_, list) => {
            let rows = list.len().min(PICK_ROWS);
            egui::ScrollArea::vertical()
                .id_salt("release_files")
                .max_height(30.0 * rows as f32)
                .min_scrolled_height(30.0 * rows as f32)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.set_width(w - 12.0);
                    ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
                    for c in list {
                        let mut on = d.files.contains(&c.key);
                        let label = if c.pre_only { format!("{}  (pre-release)", c.name) } else { c.name.clone() };
                        if toggle(ui, &mut on, &label).changed() {
                            d.files.retain(|k| *k != c.key);
                            if on {
                                d.files.push(c.key.clone());
                            }
                        }
                    }
                });
        }
    }
    ui.add_space(4.0);
    note(
        ui,
        tr(
            "Only the newest release is kept, in latest/, and the newest pre-release in prerelease/. A new one is checked (sha256) before the old one is deleted.",
            "Si tiene solo l'ultima release, in latest/, e l'ultima pre-release in prerelease/. La nuova viene controllata (sha256) prima di cancellare la vecchia.",
        ),
        DEEP,
    );
}
