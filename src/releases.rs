//! Files attached to a repo's releases (builds, installers), read from the
//! host's web API and downloaded with `curl`, which ships with Windows 10 and 11.
//!
//! The newest stable releases are kept, as many as the repo asks for (one by
//! default, see [`Project::keep`]): the newest in `<dir>/<tag>/`, the older ones
//! in `<dir>/OLD/<tag>/`, and the newest pre-release when it is newer than
//! those in `<dir>/prerelease/<tag>/`. `<dir>` is `Name-releases` under the
//! backup folder, or a folder picked for the repo. With [`Project::flat`] the
//! newest release's files sit loose in `<dir>` itself, so their path never
//! changes; Salvage remembers which files those are (see [`Loose`]).
//! A new release is downloaded and checked (sha256 when the host publishes it,
//! size otherwise) before an old one is deleted.
//!
//! The auto-updater follows one release file instead and unpacks each new
//! build over a folder (see [`crate::extract`]).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::git::{curl, curl_with_input, stopped_msg, Bus, Event, LogKind};
use crate::lang::tr;
use crate::extract;
use crate::model::{sanitize_name, Project, Update};

/// Saved in a project's platforms: every file, whatever system it is for.
pub const ALL: &str = "all";

/// Token from the settings for GitHub's API (empty: none). Without one GitHub
/// answers 60 requests per hour per address; with one, 5000.
static GITHUB_TOKEN: Mutex<String> = Mutex::new(String::new());

pub fn set_github_token(token: &str) {
    if let Ok(mut t) = GITHUB_TOKEN.lock() {
        *t = token.trim().to_string();
    }
}

fn github_token() -> String {
    GITHUB_TOKEN.lock().map(|t| t.clone()).unwrap_or_default()
}

/// Tries per file before giving up until the next sync.
const DOWNLOAD_TRIES: u32 = 5;

struct Asset {
    name: String,
    url: String,
    size: Option<u64>,
    sha256: Option<String>,
}

struct Release {
    tag: String,
    pre: bool,
    date: String,
    assets: Vec<Asset>,
}

/// What a release file is built for, guessed from its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Windows,
    Linux,
    Android,
    MacOs,
    /// Nothing in the name says which system it is for.
    Other,
}

impl Platform {
    pub const ALL: [Platform; 5] = [Platform::Windows, Platform::Linux, Platform::Android, Platform::MacOs, Platform::Other];

    /// Saved in the project's list of chosen platforms.
    pub fn key(self) -> &'static str {
        match self {
            Platform::Windows => "windows",
            Platform::Linux => "linux",
            Platform::Android => "android",
            Platform::MacOs => "macos",
            Platform::Other => "other",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Platform::Windows => "Windows",
            Platform::Linux => "Linux",
            Platform::Android => "Android",
            Platform::MacOs => "macOS",
            Platform::Other => tr("other", "altro"),
        }
    }
}

/// Which system a release file is for, from the words in its name and its
/// extension. `None` for checksums and signatures: they are only used to check
/// the other files, never offered.
pub fn platform(name: &str) -> Option<Platform> {
    let n = name.to_lowercase();
    let ext = n.rsplit_once('.').map_or("", |(_, e)| e);
    if matches!(ext, "sha256" | "sha256sum" | "sha512" | "sha512sum" | "sha1" | "md5" | "sig" | "asc" | "minisig" | "pem" | "sbom")
        || n.contains("sha256sums")
        || n.contains("sha512sums")
        || n.contains("checksums")
    {
        return None;
    }
    let words: Vec<&str> = n.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |list: &[&str]| words.iter().any(|w| list.contains(w));
    // Android before Linux: `aarch64-linux-android` is an Android build.
    let found = if has(&["android", "apk"]) {
        Platform::Android
    } else if has(&["macos", "mac", "osx", "darwin", "apple", "dmg"]) {
        Platform::MacOs
    } else if has(&["windows", "win", "win32", "win64", "msvc", "mingw", "mingw32", "mingw64", "exe", "msi", "msix", "appx", "msixbundle", "appxbundle"]) {
        Platform::Windows
    } else if has(&["linux", "appimage", "deb", "rpm", "flatpak", "snap", "musl", "ubuntu", "debian", "fedora"]) {
        Platform::Linux
    } else if ext == "aab" {
        Platform::Android
    } else if ext == "pkg" {
        Platform::MacOs
    } else {
        Platform::Other
    };
    Some(found)
}

/// One line of the picker in the add/edit dialogs: a platform and the files
/// the newest releases have for it.
#[derive(Clone)]
pub struct Choice {
    pub platform: Platform,
    pub files: Vec<String>,
    /// For each file, its name without the version (see [`file_key`]).
    pub keys: Vec<String>,
}

#[derive(Clone, Copy)]
enum Api {
    GitHub,
    Gitea,
    GitLab,
}

/// `Hookshot-v2.3.1-Windows-x64.zip` in release `v2.3.1` -> `Hookshot-{v}-Windows-x64.zip`
pub fn file_key(tag: &str, name: &str) -> String {
    let mut key = name.to_string();
    if !tag.is_empty() {
        key = key.replace(tag, "{v}");
    }
    // `release-2.6` names its files `App-2.6.apk`. Only dotted versions, so
    // that a tag `v2` does not eat the 2 in `win32`.
    if let Some(core) = tag.find(|c: char| c.is_ascii_digit()).map(|i| &tag[i..]) {
        if core.contains('.') {
            key = key.replace(core, "{v}");
        }
    }
    key
}

/// Web address of the host and the repo path on it.
/// `git@github.com:a/b.git` -> ("https://github.com", "a/b")
fn web_parts(url: &str) -> Option<(String, String)> {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let (scheme, host, path) = if let Some((scheme, rest)) = u.split_once("://") {
        let (auth, path) = rest.split_once('/')?;
        let host = auth.rsplit('@').next()?;
        match scheme {
            "https" | "http" => (scheme, host, path),
            // An ssh port is not the web port.
            "ssh" | "git" => ("https", host.split(':').next()?, path),
            _ => return None,
        }
    } else {
        // scp form: user@host:path
        let (auth, path) = u.split_once(':')?;
        ("https", auth.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/');
    if host.is_empty() || !path.contains('/') {
        return None;
    }
    Some((format!("{scheme}://{host}"), path.to_string()))
}

fn encode(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn api_url(api: Api, base: &str, path: &str) -> String {
    match api {
        Api::GitHub => format!("https://api.github.com/repos/{path}/releases?per_page=30"),
        Api::Gitea => format!("{base}/api/v1/repos/{path}/releases?limit=30"),
        Api::GitLab => format!("{base}/api/v4/projects/{}/releases?per_page=30", encode(path)),
    }
}

fn parse(api: Api, r: &Value) -> Option<Release> {
    let tag = r["tag_name"].as_str().filter(|t| !t.is_empty())?.to_string();
    if r["draft"].as_bool() == Some(true) {
        return None;
    }
    let list = match api {
        Api::GitHub | Api::Gitea => r["assets"].as_array(),
        Api::GitLab => r["assets"]["links"].as_array(),
    };
    let assets = list
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let url = match api {
                Api::GitHub | Api::Gitea => a["browser_download_url"].as_str(),
                Api::GitLab => a["direct_asset_url"].as_str().or(a["url"].as_str()),
            }?;
            Some(Asset {
                name: a["name"].as_str()?.to_string(),
                url: url.to_string(),
                size: a["size"].as_u64(),
                sha256: a["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase),
            })
        })
        .collect();
    let date = ["published_at", "released_at", "created_at"]
        .iter()
        .find_map(|k| r[*k].as_str())
        .unwrap_or_default()
        .to_string();
    Some(Release {
        tag,
        pre: r["prerelease"].as_bool() == Some(true),
        date,
        assets,
    })
}

/// Turn curl's wording into something a person can act on.
fn humanize(err: &str) -> String {
    let e = err.trim().trim_start_matches("curl:").trim();
    if e.contains("error: 401") {
        tr("GitHub refused the token in the settings (wrong, expired or revoked)", "GitHub rifiuta il token nelle impostazioni (sbagliato, scaduto o revocato)").into()
    } else if e.contains("error: 404") {
        tr("releases not found (repo removed, made private, or the host has no releases)", "release non trovate (repo rimosso, reso privato, o l'host non ha release)").into()
    } else if e.contains("error: 403") || e.contains("error: 429") {
        if github_token().is_empty() {
            tr(
                "the host refused: too many requests, try again in an hour (for GitHub: add a token in the settings)",
                "l'host rifiuta: troppe richieste, riprova tra un'ora (per GitHub: aggiungi un token nelle impostazioni)",
            )
            .into()
        } else {
            tr("the host refused: too many requests, try again in an hour", "l'host rifiuta: troppe richieste, riprova tra un'ora").into()
        }
    } else if e.contains("Could not resolve host") {
        tr("host unreachable (domain gone or no network)", "host non raggiungibile (dominio sparito o niente rete)").into()
    } else if e.is_empty() {
        tr("curl failed without saying why", "curl ha fallito senza spiegazioni").into()
    } else {
        e.to_string()
    }
}

/// The newest releases the host lists, newest first.
fn fetch(bus: &Bus, tag: &str, url: &str) -> Result<Vec<Release>, String> {
    let (base, path) = web_parts(url).ok_or_else(|| {
        tr("this link has no web page to read releases from", "questo link non ha una pagina web da cui leggere le release").to_string()
    })?;
    let host = base.split_once("://").map_or(base.as_str(), |(_, h)| h).to_lowercase();
    let apis: &[Api] = if host == "github.com" || host == "www.github.com" {
        &[Api::GitHub]
    } else if host.contains("gitlab") {
        &[Api::GitLab, Api::Gitea]
    } else {
        &[Api::Gitea, Api::GitLab]
    };
    let mut last_err = String::new();
    for &api in apis {
        let api_link = api_url(api, &base, &path);
        let mut args = vec!["-fsSL", "-H", "Accept: application/json", "-A", "Salvage"];
        let token = if matches!(api, Api::GitHub) { github_token() } else { String::new() };
        // The token only ever goes to api.github.com, through stdin.
        let done = if token.is_empty() {
            args.push(&api_link);
            curl(bus, tag, None, &args, Some(Duration::from_secs(40)))
        } else {
            args.extend(["-H", "@-", api_link.as_str()]);
            curl_with_input(bus, tag, &args, Some(Duration::from_secs(40)), &format!("Authorization: Bearer {token}
"))
        };
        if !done.ok {
            last_err = humanize(&done.err);
            continue;
        }
        let Ok(Value::Array(list)) = serde_json::from_str::<Value>(&done.stdout) else {
            last_err = tr("the host answered with something that is not a list of releases", "l'host ha risposto con qualcosa che non è una lista di release").into();
            continue;
        };
        let mut releases: Vec<Release> = list.iter().filter_map(|r| parse(api, r)).collect();
        releases.sort_by(|a, b| b.date.cmp(&a.date));
        return Ok(releases);
    }
    Err(last_err)
}

/// The newest stable release, and the newest pre-release if it came out after it.
fn pick(releases: &[Release]) -> (Option<&Release>, Option<&Release>) {
    let stable = releases.iter().position(|r| !r.pre);
    let pre = releases.iter().position(|r| r.pre).filter(|&i| stable.is_none_or(|s| i < s));
    (stable.map(|i| &releases[i]), pre.map(|i| &releases[i]))
}

/// What the dialogs offer to pick: every file of the newest release, then the
/// ones only the newest pre-release has. Sent back as [`Event::Files`].
pub fn list_files(bus: &Bus, url: &str) {
    let result = fetch(bus, tr("releases", "release"), url).and_then(|releases| {
        let (stable, pre) = pick(&releases);
        let mut out: Vec<Choice> = Platform::ALL.iter().map(|&platform| Choice { platform, files: Vec::new(), keys: Vec::new() }).collect();
        let mut seen: Vec<String> = Vec::new();
        for r in [stable, pre].into_iter().flatten() {
            for a in &r.assets {
                let Some(pl) = platform(&a.name) else {
                    continue;
                };
                // The pre-release often has the same files under a newer version.
                let key = file_key(&r.tag, &a.name);
                if !seen.contains(&key) {
                    seen.push(key.clone());
                    if let Some(c) = out.iter_mut().find(|c| c.platform == pl) {
                        c.files.push(a.name.clone());
                        c.keys.push(key.clone());
                    }
                }
            }
        }
        if seen.is_empty() {
            Err(tr("this repo has no release with files attached", "questo repo non ha release con file allegati").into())
        } else {
            Ok(out)
        }
    });
    bus.send(Event::Files { url: url.to_string(), result });
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_sums_file(name: &str) -> bool {
    let n = name.to_lowercase();
    n.ends_with(".sha256") || n.ends_with(".sha256sum") || n.contains("sha256sums") || n.contains("checksums")
}

/// Read `sha256sum` style lines (`<hash>  <name>`) into name -> hash.
/// `App.zip.sha256` may hold the hash alone: it then belongs to `App.zip`.
fn parse_sums(file: &str, text: &str, out: &mut HashMap<String, String>) {
    let lone = [".sha256sum", ".sha256"].iter().find_map(|s| file.strip_suffix(s));
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(hash) = words.next().filter(|w| is_hex64(w)) else {
            continue;
        };
        let name = words.last().map(|w| w.trim_start_matches('*').rsplit(['/', '\\']).next().unwrap_or(w).to_string()).or(lone.map(str::to_string));
        if let Some(name) = name {
            out.insert(name, hash.to_lowercase());
        }
    }
}

/// Checksums published as files next to the release (for hosts whose API has none).
fn load_sums(bus: &Bus, tag: &str, rel: &Release) -> HashMap<String, String> {
    let mut sums = HashMap::new();
    for a in rel.assets.iter().filter(|a| is_sums_file(&a.name) && a.size.is_none_or(|s| s < 1 << 20)) {
        let done = curl(bus, tag, None, &["-fsSL", "-A", "Salvage", &a.url], Some(Duration::from_secs(40)));
        if done.ok {
            parse_sums(&a.name, &done.stdout, &mut sums);
        }
    }
    sums
}

/// Download one file into `dir` and check it before it takes its real name.
fn download(bus: &Bus, p: &Project, rel: &Release, a: &Asset, dir: &Path, sums: &mut Option<HashMap<String, String>>) -> Result<(), String> {
    let tag = p.name.as_str();
    if bus.cancel.load(Ordering::Relaxed) {
        return Err(stopped_msg().into());
    }
    let file = dir.join(sanitize_name(&a.name));
    let part = dir.join(format!("{}.part", sanitize_name(&a.name)));
    let target = part.to_string_lossy().to_string();
    bus.log(tag, LogKind::Info, format!("{} / {}", rel.tag, a.name));
    // Big files over a long transfer often lose the connection halfway
    // (`curl: (56) Recv failure: Connection was reset`). Resume from what is
    // already in the `.part` instead of starting over; the sha256 check below
    // proves the pieces add up. The `.part` also survives to the next sync.
    let part_len = || std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let have = part_len();
        if have > 0 && a.size == Some(have) {
            break;
        }
        let mut args = vec!["-fL", "-#", "--retry", "2", "--speed-limit", "1024", "--speed-time", "60", "-A", "Salvage"];
        if have > 0 {
            args.extend(["-C", "-"]);
        }
        args.extend(["-o", target.as_str(), a.url.as_str()]);
        let done = curl(bus, tag, Some(p.id), &args, None);
        if done.ok {
            break;
        }
        if bus.cancel.load(Ordering::Relaxed) {
            return Err(stopped_msg().into());
        }
        if attempt >= DOWNLOAD_TRIES {
            return Err(format!("{}: {}", a.name, humanize(&done.err)));
        }
        let mib = part_len() as f64 / (1024.0 * 1024.0);
        bus.log(
            tag,
            LogKind::Info,
            t!(
                "connection dropped at {mib:.1} MiB, resuming (attempt {}/{DOWNLOAD_TRIES})",
                "connessione caduta a {mib:.1} MiB, riprendo (tentativo {}/{DOWNLOAD_TRIES})",
                attempt + 1
            ),
        );
        std::thread::sleep(Duration::from_secs(2));
    }

    let expected = a.sha256.clone().or_else(|| sums.get_or_insert_with(|| load_sums(bus, tag, rel)).get(&a.name).cloned());
    let bad = |why: String| {
        let _ = std::fs::remove_file(&part);
        Err(format!("{}: {why}", a.name))
    };
    let how = if let Some(want) = expected {
        let got = sha256_file(&part).map_err(|e| e.to_string())?;
        if got != want {
            return bad(tr("sha256 does not match, file discarded", "lo sha256 non corrisponde, file scartato").into());
        }
        "sha256"
    } else if let Some(size) = a.size {
        let len = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if len != size {
            return bad(t!("{len} bytes instead of {size}, file discarded", "{len} byte invece di {size}, file scartato"));
        }
        tr("size, the host publishes no sha256", "dimensione, l'host non pubblica lo sha256")
    } else {
        tr("not checked: the host gives no sha256 nor size", "non controllato: l'host non dà né sha256 né dimensione")
    };
    std::fs::rename(&part, &file).map_err(|e| format!("{}: {e}", a.name))?;
    bus.log(tag, LogKind::Good, t!("{} ok ({how})", "{} ok ({how})", a.name));
    Ok(())
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default()
}

fn remove_all(bus: &Bus, tag: &str, dir: &Path) -> bool {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {
            bus.log(tag, LogKind::Info, t!("removed {}", "rimossa {}", dir.display()));
            true
        }
        Err(e) => {
            bus.log(tag, LogKind::Err, t!("cannot remove {}: {e}", "non riesco a rimuovere {}: {e}", dir.display()));
            false
        }
    }
}

/// Inside the releases folder: older stable releases, the pre-release, and
/// where stable releases were kept before 0.4.
const OLD: &str = "OLD";
const PRE: &str = "prerelease";
const LEGACY: &str = "latest";
/// Where releases folders listed their own folders before 0.5: moved into
/// [`owned_path`] on first sight.
const OLD_OWNED_FILE: &str = ".salvage-releases.json";

/// One file for every repo, kept with the config: for each releases folder,
/// the release folders in it that Salvage made.
fn owned_path() -> PathBuf {
    crate::model::config_path().with_file_name("releases.json")
}

/// One file for every repo, kept with the config: for each releases folder,
/// the release whose files sit loose in it (see [`Project::flat`]).
fn loose_path() -> PathBuf {
    crate::model::config_path().with_file_name("release-loose.json")
}

/// The release whose files Salvage put loose in a releases folder.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Loose {
    tag: String,
    files: Vec<String>,
}

fn read_loose(path: &Path) -> HashMap<String, Loose> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Releases sync in parallel: one at a time reads and rewrites [`owned_path`]
/// and [`loose_path`].
static OWNED_LOCK: Mutex<()> = Mutex::new(());

fn read_owned(path: &Path) -> HashMap<String, Vec<String>> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// The release folders Salvage made, listed in [`owned_path`] under the
/// releases folder. In a folder the user picked, only these are ever moved
/// or deleted: anything else in there is left alone.
struct Owned {
    dir: PathBuf,
    /// The key of `dir` in [`owned_path`].
    id: String,
    /// `Name-releases` under the backup folder is Salvage's as a whole.
    all: bool,
    list: Vec<String>,
    /// The release loose in `dir`, if any.
    loose: Option<Loose>,
}

impl Owned {
    fn load(bus: &Bus, tag: &str, dir: &Path, all: bool) -> Self {
        let id = dir.to_string_lossy().replace('\\', "/");
        let (list, loose) = {
            let _guard = OWNED_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            (read_owned(&owned_path()).remove(&id).unwrap_or_default(), read_loose(&loose_path()).remove(&id))
        };
        let mut owned = Self { dir: dir.to_path_buf(), id, all, list, loose };
        // The list of an older Salvage, in the folder itself: it joins the
        // central one, then goes once that is written.
        let old = dir.join(OLD_OWNED_FILE);
        if let Some(found) = std::fs::read_to_string(&old).ok().and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok()) {
            for key in found {
                if !owned.list.contains(&key) {
                    owned.list.push(key);
                }
            }
            match owned.save() {
                Ok(()) => {
                    if std::fs::remove_file(&old).is_ok() {
                        bus.log(tag, LogKind::Info, t!("moved the list of {} into {}", "spostato l'elenco di {} in {}", old.display(), owned_path().display()));
                    }
                }
                Err(e) => bus.log(tag, LogKind::Err, e),
            }
        }
        owned
    }

    fn key(&self, path: &Path) -> Option<String> {
        path.strip_prefix(&self.dir).ok().map(|r| r.to_string_lossy().replace('\\', "/"))
    }

    fn has(&self, path: &Path) -> bool {
        let Some(key) = self.key(path) else {
            return false;
        };
        // A side folder for a download in progress is always Salvage's.
        let incoming = path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".incoming-"));
        self.all || incoming || self.list.contains(&key)
    }

    fn add(&mut self, path: &Path) {
        if let Some(key) = self.key(path) {
            if !self.list.contains(&key) {
                self.list.push(key);
            }
        }
    }

    fn forget(&mut self, path: &Path) {
        if let Some(key) = self.key(path) {
            self.list.retain(|k| *k != key);
        }
    }

    /// Rewrite this folder's entry, leaving the other folders' as they are.
    /// A folder that is not there (a drive unplugged) keeps its entry.
    fn save(&self) -> Result<(), String> {
        if !self.dir.is_dir() {
            return Ok(());
        }
        let _guard = OWNED_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = owned_path();
        let mut all = read_owned(&path);
        if self.list.is_empty() {
            all.remove(&self.id);
        } else {
            all.insert(self.id.clone(), self.list.clone());
        }
        let fail = |e: std::io::Error| t!("cannot write {}: {e}", "non riesco a scrivere {}: {e}", path.display());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(fail)?;
        }
        let text = serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?;
        write_json(&path, &text).map_err(fail)?;

        let path = loose_path();
        let mut all = read_loose(&path);
        if all.get(&self.id) == self.loose.as_ref() {
            return Ok(());
        }
        match &self.loose {
            Some(l) => all.insert(self.id.clone(), l.clone()),
            None => all.remove(&self.id),
        };
        let text = serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?;
        write_json(&path, &text).map_err(|e| t!("cannot write {}: {e}", "non riesco a scrivere {}: {e}", path.display()))
    }
}

/// Through a temp file, so a crash mid-write cannot lose the list.
fn write_json(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn clash(path: &Path) -> String {
    t!(
        "{} is already there and was not made by Salvage: rename it, or pick another folder",
        "{} c'è già e non l'ha creato Salvage: rinominalo, o scegli un'altra cartella",
        path.display()
    )
}

/// Move the files `names` from folder `from` into folder `to`, replacing
/// files of the same name. Returns the names done (moved, or already gone)
/// and the error that stopped it, if one did.
fn move_files(names: &[String], from: &Path, to: &Path) -> (Vec<String>, Option<String>) {
    let mut done = Vec::new();
    for n in names {
        let src = from.join(n);
        if src.exists() {
            if let Err(e) = std::fs::rename(&src, to.join(n)) {
                return (done, Some(t!("cannot move {}: {e}", "non riesco a spostare {}: {e}", src.display())));
            }
        }
        done.push(n.clone());
    }
    (done, None)
}

/// Move the release loose in the releases folder into `into`, keeping track
/// of what is still loose if a file cannot move.
fn unloose(bus: &Bus, tag: &str, dir: &Path, into: &Path, owned: &mut Owned) -> Result<(), String> {
    let Some(loose) = owned.loose.clone() else {
        return Ok(());
    };
    if into.exists() && !owned.has(into) {
        let e = clash(into);
        bus.log(tag, LogKind::Err, e.clone());
        return Err(e);
    }
    std::fs::create_dir_all(into).map_err(|e| t!("cannot create {}: {e}", "non riesco a creare {}: {e}", into.display()))?;
    owned.add(into);
    let (done, err) = move_files(&loose.files, dir, into);
    let left: Vec<String> = loose.files.into_iter().filter(|f| !done.contains(f)).collect();
    owned.loose = (!left.is_empty()).then(|| Loose { tag: loose.tag.clone(), files: left });
    if let Some(e) = err {
        bus.log(tag, LogKind::Err, e.clone());
        return Err(e);
    }
    bus.log(tag, LogKind::Info, t!("moved {} to {}", "spostata {} in {}", loose.tag, into.display()));
    Ok(())
}

/// Keep the files of `rel`, the newest stable release, loose in `dir`. A new
/// release is completed in `dir/<tag>/` first (checked like any other); only
/// then does the release loose until now move to `old/<tag>/`, and the new
/// files take its place.
fn sync_flat(bus: &Bus, p: &Project, dir: &Path, old: &Path, rel: &Release, owned: &mut Owned) -> Result<Option<PathBuf>, String> {
    let tag = p.name.as_str();
    let wanted = wanted(p, rel);
    if wanted.is_empty() {
        bus.log(tag, LogKind::Info, t!("{}: none of the chosen files is in this release", "{}: nessuno dei file scelti è in questa release", rel.tag));
        return Ok(None);
    }
    let folder_name = sanitize_name(&rel.tag);
    // A file in the folder that Salvage did not put there.
    let taken = |owned: &Owned, name: &str| dir.join(name).exists() && !owned.loose.as_ref().is_some_and(|l| l.files.iter().any(|f| f == name));

    if owned.loose.as_ref().is_some_and(|l| l.tag == rel.tag) && !dir.join(&folder_name).is_dir() {
        // Same release: fetch what is missing, drop what is no longer chosen.
        let mut sums = None;
        let mut first_err = None;
        let names: Vec<String> = wanted.iter().map(|a| sanitize_name(&a.name)).collect();
        for (a, name) in wanted.iter().zip(&names) {
            let len = std::fs::metadata(dir.join(name)).ok().map(|m| m.len());
            if !taken(owned, name) && len.is_some() && (a.size.is_none() || len == a.size) {
                continue;
            }
            let result = if taken(owned, name) { Err(clash(&dir.join(name))) } else { download(bus, p, rel, a, dir, &mut sums) };
            match result {
                Ok(()) => {
                    if let Some(l) = owned.loose.as_mut().filter(|l| !l.files.contains(name)) {
                        l.files.push(name.clone());
                    }
                }
                Err(e) => {
                    bus.log(tag, LogKind::Err, e.clone());
                    first_err.get_or_insert(e);
                }
            }
        }
        if let Some(l) = owned.loose.as_mut() {
            l.files.retain(|f| {
                if names.contains(f) {
                    return true;
                }
                if std::fs::remove_file(dir.join(f)).is_ok() {
                    bus.log(tag, LogKind::Info, t!("removed {f}: no longer chosen", "rimosso {f}: non più scelto"));
                }
                false
            });
        }
        return first_err.map_or(Ok(Some(dir.to_path_buf())), Err);
    }

    // A new release, or one already on disk in a folder of its own (the
    // newest from before the repo kept it loose, or an old one back on top).
    place(bus, tag, &folder_name, dir, &[old, &dir.join(LEGACY)], owned);
    let Some(folder) = sync_release(bus, p, dir, rel, owned)? else {
        return Ok(None);
    };
    let files: Vec<String> = std::fs::read_dir(&folder)
        .map_err(|e| format!("{}: {e}", folder.display()))?
        .flatten()
        .filter(|f| f.path().is_file())
        .map(|f| f.file_name().to_string_lossy().to_string())
        .collect();
    // Nothing moves unless every new file can take its place.
    if let Some(f) = files.iter().find(|f| taken(owned, f)) {
        let e = clash(&dir.join(f));
        bus.log(tag, LogKind::Err, e.clone());
        return Err(e);
    }
    // The release loose until now goes to OLD/<tag>/; the rest of the sync
    // keeps it there or deletes it, as for any old release. A half-moved
    // copy of this same release is simply replaced.
    if let Some(prev) = owned.loose.as_ref().filter(|l| l.tag != rel.tag).map(|l| sanitize_name(&l.tag)) {
        unloose(bus, tag, dir, &old.join(prev), owned)?;
    }
    let (done, err) = move_files(&files, &folder, dir);
    let mut now = owned.loose.take().map(|l| l.files).unwrap_or_default();
    now.extend(done.into_iter().filter(|f| !now.contains(f)).collect::<Vec<_>>());
    owned.loose = (!now.is_empty()).then(|| Loose { tag: rel.tag.clone(), files: now });
    if let Some(e) = err {
        bus.log(tag, LogKind::Err, e.clone());
        return Err(e);
    }
    if std::fs::remove_dir(&folder).is_ok() {
        owned.forget(&folder);
    }
    bus.log(tag, LogKind::Info, t!("{} now loose in {}", "{} ora direttamente in {}", rel.tag, dir.display()));
    Ok(Some(dir.to_path_buf()))
}

/// The files of `rel` for the systems the repo keeps.
fn wanted<'a>(p: &Project, rel: &'a Release) -> Vec<&'a Asset> {
    rel.assets
        .iter()
        .filter(|a| platform(&a.name).is_some_and(|pl| p.platforms.iter().any(|k| k == ALL || k == pl.key())))
        .collect()
}

/// Bring `slot/<tag>/` to the chosen files of `rel`. Other releases in the
/// slot are left alone: the caller decides which of them are outdated.
/// Returns the release folder, or `None` when `rel` has none of the chosen files.
fn sync_release(bus: &Bus, p: &Project, slot: &Path, rel: &Release, owned: &mut Owned) -> Result<Option<PathBuf>, String> {
    let tag = p.name.as_str();
    let wanted = wanted(p, rel);
    if wanted.is_empty() {
        bus.log(tag, LogKind::Info, t!("{}: none of the chosen files is in this release", "{}: nessuno dei file scelti è in questa release", rel.tag));
        return Ok(None);
    }
    let current = slot.join(sanitize_name(&rel.tag));
    let mut sums = None;

    if current.is_dir() && !owned.has(&current) {
        let e = t!(
            "{} is already there and was not made by Salvage: rename it, or pick another folder",
            "{} c'è già e non l'ha creata Salvage: rinominala, o scegli un'altra cartella",
            current.display()
        );
        bus.log(tag, LogKind::Err, e.clone());
        return Err(e);
    }
    if current.is_dir() {
        // Same release: fetch what is missing, drop what is no longer chosen.
        let mut first_err = None;
        for a in &wanted {
            let len = std::fs::metadata(current.join(sanitize_name(&a.name))).ok().map(|m| m.len());
            if len.is_some() && (a.size.is_none() || len == a.size) {
                continue;
            }
            if let Err(e) = download(bus, p, rel, a, &current, &mut sums) {
                bus.log(tag, LogKind::Err, e.clone());
                first_err.get_or_insert(e);
            }
        }
        // A `.part` of a chosen file is a download to resume, not a leftover.
        let keep: Vec<String> = wanted.iter().flat_map(|a| [sanitize_name(&a.name), format!("{}.part", sanitize_name(&a.name))]).collect();
        for f in std::fs::read_dir(&current).into_iter().flatten().flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            if f.path().is_file() && !keep.contains(&name) && std::fs::remove_file(f.path()).is_ok() {
                bus.log(tag, LogKind::Info, t!("removed {name}: no longer chosen", "rimosso {name}: non più scelto"));
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
    } else {
        // New release: everything goes into a side folder first. Old releases
        // are deleted only once every new file has passed its check.
        bus.log(tag, LogKind::Info, t!("new release {}", "nuova release {}", rel.tag));
        // A folder left by an interrupted sync is reused: its finished files
        // already passed the check, its `.part` files resume.
        let incoming = slot.join(format!(".incoming-{}", sanitize_name(&rel.tag)));
        std::fs::create_dir_all(&incoming).map_err(|e| t!("cannot create {}: {e}", "non riesco a creare {}: {e}", incoming.display()))?;
        for a in &wanted {
            let len = std::fs::metadata(incoming.join(sanitize_name(&a.name))).ok().map(|m| m.len());
            if len.is_some() && (a.size.is_none() || len == a.size) {
                continue;
            }
            if let Err(e) = download(bus, p, rel, a, &incoming, &mut sums) {
                bus.log(tag, LogKind::Err, t!("{e}; the old releases stay, the next sync picks up from here", "{e}; le release vecchie restano, la prossima sincronizzazione riprende da qui"));
                return Err(e);
            }
        }
        std::fs::rename(&incoming, &current).map_err(|e| t!("cannot move {}: {e}", "non riesco a spostare {}: {e}", incoming.display()))?;
    }
    owned.add(&current);
    Ok(Some(current))
}

/// Delete the release folders in `slot` that are not in `keep`. The `OLD`,
/// `prerelease` and `latest` folders themselves are not releases.
fn prune(bus: &Bus, tag: &str, slot: &Path, keep: &[PathBuf], owned: &mut Owned) {
    for old in subdirs(slot) {
        let name = old.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if keep.contains(&old) || [OLD, PRE, LEGACY].contains(&name.as_str()) || !owned.has(&old) {
            continue;
        }
        if remove_all(bus, tag, &old) {
            owned.forget(&old);
        }
    }
}

/// Move release folder `name` into `slot` when an earlier sync left it in
/// another one (the newest becomes old, or a pre-0.4 `latest/` folder).
fn place(bus: &Bus, tag: &str, name: &str, slot: &Path, from: &[&Path], owned: &mut Owned) {
    let target = slot.join(name);
    if target.exists() {
        return;
    }
    for s in from {
        let src = s.join(name);
        if !src.is_dir() || !owned.has(&src) {
            continue;
        }
        let _ = std::fs::create_dir_all(slot);
        match std::fs::rename(&src, &target) {
            Ok(()) => {
                owned.forget(&src);
                owned.add(&target);
                bus.log(tag, LogKind::Info, t!("moved {name} to {}", "spostata {name} in {}", slot.display()));
            }
            Err(e) => bus.log(tag, LogKind::Err, t!("cannot move {}: {e}", "non riesco a spostare {}: {e}", src.display())),
        }
        return;
    }
}

/// The newest `n` stable releases that have some of the chosen files, newest first.
fn stables<'a>(p: &Project, releases: &'a [Release], n: usize) -> Vec<&'a Release> {
    releases.iter().filter(|r| !r.pre && !wanted(p, r).is_empty()).take(n).collect()
}

/// Keep the newest stable release in `<dir>/<tag>/`, the older ones the repo
/// asks for in `<dir>/OLD/<tag>/` and the newest pre-release in
/// `<dir>/prerelease/<tag>/`, with the chosen files.
fn archive(bus: &Bus, p: &Project, root: &Path, releases: &[Release]) -> Result<(), String> {
    let tag = p.name.as_str();
    let (stable, pre) = pick(releases);
    let kept = stables(p, releases, p.keep());
    let dir = p.releases_dir(root);
    let mut owned = Owned::load(bus, tag, &dir, p.release_dir.is_none());
    let (old, pre_dir, legacy) = (dir.join(OLD), dir.join(PRE), dir.join(LEGACY));
    let mut first_err = None;

    // The repo no longer keeps the newest release loose: its files go back
    // into `<dir>/<tag>/`, which the steps below handle as usual.
    if !p.flat {
        if let Some(name) = owned.loose.as_ref().map(|l| sanitize_name(&l.tag)) {
            if let Err(e) = unloose(bus, tag, &dir, &dir.join(name), &mut owned) {
                first_err.get_or_insert(e);
            }
        }
    }

    // No stable release upstream with the chosen files (deleted, or never
    // made): keep what is on disk.
    if stable.is_some() && kept.is_empty() {
        bus.log(tag, LogKind::Info, tr("none of the chosen files is in the stable releases", "nessuno dei file scelti è nelle release stabili"));
    }
    if !kept.is_empty() && first_err.is_none() {
        let mut done = Vec::new();
        for (i, rel) in kept.iter().enumerate() {
            let result = if i == 0 && p.flat {
                sync_flat(bus, p, &dir, &old, rel, &mut owned)
            } else {
                let slot = if i == 0 { &dir } else { &old };
                place(bus, tag, &sanitize_name(&rel.tag), slot, &[&dir, &old, &legacy], &mut owned);
                sync_release(bus, p, slot, rel, &mut owned)
            };
            match result {
                Ok(Some(folder)) => done.push(folder),
                Ok(None) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                    // Without the newest, leave the older ones where they are.
                    if i == 0 {
                        break;
                    }
                }
            }
        }
        // Old releases go only once every kept one is complete, so a failed
        // download never leaves fewer releases on disk than before.
        if first_err.is_none() {
            for slot in [&dir, &old, &legacy] {
                prune(bus, tag, slot, &done, &mut owned);
            }
            // Only when empty: a folder of the user's with that name stays.
            let _ = std::fs::remove_dir(&old);
            let _ = std::fs::remove_dir(&legacy);
        }
    }
    match pre {
        Some(rel) => match sync_release(bus, p, &pre_dir, rel, &mut owned) {
            Ok(Some(folder)) => prune(bus, tag, &pre_dir, &[folder], &mut owned),
            Ok(None) => {}
            Err(e) => {
                first_err.get_or_insert(e);
            }
        },
        // A stable release came out after the last pre-release: that one is outdated.
        None if stable.is_some() => {
            prune(bus, tag, &pre_dir, &[], &mut owned);
            let _ = std::fs::remove_dir(&pre_dir);
        }
        None => {}
    }
    if let Err(e) = owned.save() {
        bus.log(tag, LogKind::Err, e);
    }
    match first_err {
        Some(e) => Err(e),
        None => {
            let latest = if kept.is_empty() { "-".to_string() } else { kept.iter().map(|r| r.tag.as_str()).collect::<Vec<_>>().join(", ") };
            let pre = pre.map_or("-".to_string(), |r| r.tag.clone());
            bus.log(tag, LogKind::Good, t!("releases: kept {latest}, pre-release {pre}", "release: tenute {latest}, pre-release {pre}"));
            Ok(())
        }
    }
}

/// In the folder the auto-updater keeps: its download and unpacked files
/// while it works, and what it last installed.
const UPDATE_WORK: &str = ".salvage-update";
const UPDATE_STATE: &str = ".salvage-update.json";

#[derive(Serialize, Deserialize, Default)]
struct Installed {
    tag: String,
    file: String,
    /// A file copied as it is (not an archive): replaced by the next one.
    #[serde(default)]
    lone: bool,
}

/// The words of a file name that stay the same from one build to the next:
/// version numbers, `rc2`, commit hashes and the like are left out.
fn shape(name: &str) -> Vec<String> {
    let changing = |w: &str| {
        let digits = w.chars().any(|c| c.is_ascii_digit());
        let tail = |pre: &str| w.strip_prefix(pre).is_some_and(|t| t.chars().all(|c| c.is_ascii_digit()));
        w.chars().all(|c| c.is_ascii_digit())
            || ["v", "rc", "beta", "alpha", "b", "pre", "dev", "build", "r"].iter().any(|p| tail(p))
            || (digits && w.len() >= 7 && w.chars().all(|c| c.is_ascii_hexdigit()))
    };
    name.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty() && !changing(w))
        .map(str::to_string)
        .collect()
}

/// The file of `rel` the updater follows: the same name with the version
/// swapped, or else the one file whose name has the same shape.
fn update_asset<'a>(rel: &'a Release, u: &Update) -> Option<&'a Asset> {
    if let Some(a) = rel.assets.iter().find(|a| file_key(&rel.tag, &a.name) == u.file) {
        return Some(a);
    }
    let want = shape(&u.name);
    let mut same = rel.assets.iter().filter(|a| shape(&a.name) == want);
    match (same.next(), same.next()) {
        (Some(a), None) => Some(a),
        _ => None,
    }
}

/// Unpack the newest build of the chosen file over the update folder.
/// Files only in the folder (saves, settings) stay.
fn update(bus: &Bus, p: &Project, u: &Update, releases: &[Release]) -> Result<(), String> {
    let tag = p.name.as_str();
    let Some(rel) = releases.iter().find(|r| u.pre || !r.pre) else {
        bus.log(tag, LogKind::Info, tr("auto-update: no release yet", "aggiornamento automatico: ancora nessuna release"));
        return Ok(());
    };
    let Some(a) = update_asset(rel, u) else {
        bus.log(
            tag,
            LogKind::Info,
            t!("auto-update: {} has no file like {}, nothing to do", "aggiornamento automatico: {} non ha un file come {}, niente da fare", rel.tag, u.name),
        );
        return Ok(());
    };
    let dir = &u.dir;
    let state_path = dir.join(UPDATE_STATE);
    let state: Installed = std::fs::read_to_string(&state_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if state.tag == rel.tag && state.file == a.name {
        bus.log(tag, LogKind::Info, t!("auto-update: {} already on {}", "aggiornamento automatico: {} già su {}", dir.display(), rel.tag));
        return Ok(());
    }

    let work = dir.join(UPDATE_WORK);
    std::fs::create_dir_all(&work).map_err(|e| t!("cannot create {}: {e}", "non riesco a creare {}: {e}", work.display()))?;
    let name = sanitize_name(&a.name);
    // Leftovers of an earlier try: anything but this file and its `.part`.
    for f in std::fs::read_dir(&work).into_iter().flatten().flatten() {
        let n = f.file_name().to_string_lossy().to_string();
        if f.path().is_dir() {
            let _ = std::fs::remove_dir_all(f.path());
        } else if n != name && n != format!("{name}.part") {
            let _ = std::fs::remove_file(f.path());
        }
    }
    let file = work.join(&name);
    let len = std::fs::metadata(&file).ok().map(|m| m.len());
    if !(len.is_some() && (a.size.is_none() || len == a.size)) {
        download(bus, p, rel, a, &work, &mut None)?;
    }

    let stage = work.join("unpacked");
    let lone = extract::kind(&a.name).is_none();
    let n = if lone {
        let _ = std::fs::create_dir_all(&stage);
        std::fs::copy(&file, stage.join(&name)).map_err(|e| format!("{name}: {e}"))?;
        extract::install(&stage, dir)?
    } else {
        extract::unpack(&file, &stage)?;
        extract::install(&extract::content_root(&stage), dir)?
    };
    if lone {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir.join(&name), std::fs::Permissions::from_mode(0o755));
        }
        // The previous build of a lone file has another name: it goes.
        if state.lone && !state.file.is_empty() && sanitize_name(&state.file) != name {
            let _ = std::fs::remove_file(dir.join(sanitize_name(&state.file)));
        }
    }
    let now = Installed { tag: rel.tag.clone(), file: a.name.clone(), lone };
    if let Ok(text) = serde_json::to_string_pretty(&now) {
        let _ = std::fs::write(&state_path, text);
    }
    let _ = std::fs::remove_dir_all(&work);
    bus.log(
        tag,
        LogKind::Good,
        t!("auto-update: {} now on {} ({n} files)", "aggiornamento automatico: {} ora su {} ({n} file)", dir.display(), rel.tag),
    );
    Ok(())
}

/// Release files and auto-update of one repo, from one read of its releases.
pub fn sync(bus: &Bus, p: &Project, root: &Path) -> Result<(), String> {
    let tag = p.name.as_str();
    let releases = fetch(bus, tag, &p.url).inspect_err(|e| bus.log(tag, LogKind::Err, t!("releases: {e}", "release: {e}")))?;
    let mut first_err = None;
    if p.releases {
        if let Err(e) = archive(bus, p, root, &releases) {
            first_err.get_or_insert(e);
        }
    }
    if let Some(u) = &p.update {
        if let Err(e) = update(bus, p, u, &releases) {
            bus.log(tag, LogKind::Err, t!("auto-update: {e}", "aggiornamento automatico: {e}"));
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        let gh = Some(("https://github.com".to_string(), "a/b".to_string()));
        assert_eq!(web_parts("https://github.com/a/b.git"), gh);
        assert_eq!(web_parts("https://github.com/a/b/"), gh);
        assert_eq!(web_parts("git@github.com:a/b.git"), gh);
        assert_eq!(web_parts("ssh://git@github.com:22/a/b.git"), gh);
        assert_eq!(web_parts("http://host:3000/g/sub/r"), Some(("http://host:3000".to_string(), "g/sub/r".to_string())));
        assert_eq!(web_parts("https://host/onlyone"), None);
        assert_eq!(encode("g/sub/r"), "g%2Fsub%2Fr");
    }

    #[test]
    fn keys() {
        assert_eq!(file_key("v2.3.1", "Hookshot-v2.3.1-Windows-x64.zip"), "Hookshot-{v}-Windows-x64.zip");
        assert_eq!(file_key("v2.4.0-rc.1", "Hookshot-v2.4.0-rc.1-Windows-x64.zip"), "Hookshot-{v}-Windows-x64.zip");
        assert_eq!(file_key("release-2.6", "CrashBandicoot-2.6.apk"), "CrashBandicoot-{v}.apk");
        assert_eq!(file_key("v2", "app-win32.exe"), "app-win32.exe");
        assert_eq!(file_key("v1.0", "CrashBandicoot.exe"), "CrashBandicoot.exe");
    }

    #[test]
    fn platforms() {
        use Platform::*;
        let cases = [
            ("CrashBandicoot-2.6.apk", Some(Android)),
            ("CrashBandicoot-arm64.exe", Some(Windows)),
            ("CrashBandicoot-Linux", Some(Linux)),
            ("CrashBandicoot-Linux-arm64", Some(Linux)),
            ("CrashBandicoot.exe", Some(Windows)),
            ("Hookshot-v2.3.1-Windows-x64.zip", Some(Windows)),
            ("Hookshot-v2.3.1-x86_64.AppImage", Some(Linux)),
            ("Hookshot-x86_64.AppImage.zsync", Some(Linux)),
            ("HookshotBuilder.exe.sha256", None),
            ("SHA256SUMS", None),
            ("ripgrep-14.1.0-x86_64-pc-windows-msvc.zip", Some(Windows)),
            ("ripgrep-14.1.0-aarch64-apple-darwin.tar.gz", Some(MacOs)),
            ("ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz", Some(Linux)),
            ("ripgrep_14.1.0-1_amd64.deb", Some(Linux)),
            ("app-aarch64-linux-android.tar.gz", Some(Android)),
            ("App-1.0.dmg", Some(MacOs)),
            ("App-1.0-universal.pkg", Some(MacOs)),
            ("assets.zip", Some(Other)),
        ];
        for (name, want) in cases {
            assert_eq!(platform(name), want, "{name}");
        }
    }

    #[test]
    fn sums() {
        let h = "a".repeat(64);
        let mut m = HashMap::new();
        parse_sums("SHA256SUMS", &format!("{h}  App-win.zip\n{h} *dist/App-linux\n"), &mut m);
        parse_sums("Other.exe.sha256", &h, &mut m);
        assert_eq!(m.get("App-win.zip"), Some(&h));
        assert_eq!(m.get("App-linux"), Some(&h));
        assert_eq!(m.get("Other.exe"), Some(&h));
    }

    #[test]
    fn picks() {
        let r = |tag: &str, pre: bool| Release { tag: tag.into(), pre, date: String::new(), assets: Vec::new() };
        let list = [r("v3-rc", true), r("v2", false), r("v2-rc", true)];
        let (s, p) = pick(&list);
        assert_eq!((s.map(|r| r.tag.as_str()), p.map(|r| r.tag.as_str())), (Some("v2"), Some("v3-rc")));
        let list = [r("v2", false), r("v2-rc", true)];
        assert!(pick(&list).1.is_none());
    }

    #[test]
    fn follows_the_same_file() {
        let asset = |name: &str| Asset { name: name.into(), url: String::new(), size: None, sha256: None };
        let rel = Release {
            tag: "v0.0.4-rc1".into(),
            pre: true,
            date: String::new(),
            assets: ["Eden-Windows-v0.0.4-rc1-amd64-msvc.zip", "Eden-Windows-v0.0.4-rc1-arm64-msvc.zip", "Eden-Linux-v0.0.4-rc1-amd64.AppImage"].map(asset).into(),
        };
        let u = |name: &str| Update { dir: PathBuf::new(), file: file_key("v0.0.3", name), name: name.into(), pre: true };
        let found = |name: &str| update_asset(&rel, &u(name)).map(|a| a.name.clone());
        assert_eq!(found("Eden-Windows-v0.0.3-amd64-msvc.zip").as_deref(), Some("Eden-Windows-v0.0.4-rc1-amd64-msvc.zip"));
        assert_eq!(found("Eden-Windows-v0.0.3-arm64-msvc.zip").as_deref(), Some("Eden-Windows-v0.0.4-rc1-arm64-msvc.zip"));
        // A commit hash in the name instead of the tag.
        assert_eq!(found("Eden-Linux-a1b2c3d-amd64.AppImage").as_deref(), Some("Eden-Linux-v0.0.4-rc1-amd64.AppImage"));
        assert_eq!(found("Eden-Android.apk"), None);
    }

    #[test]
    fn keeps_newest_stables() {
        let asset = |name: &str| Asset { name: name.into(), url: String::new(), size: None, sha256: None };
        let r = |tag: &str, pre: bool, files: &[&str]| Release { tag: tag.into(), pre, date: String::new(), assets: files.iter().map(|f| asset(f)).collect() };
        let list = [
            r("v5-rc", true, &["App-win.zip"]),
            r("v4", false, &["App-linux.tar.gz"]),
            r("v3", false, &["App-win.zip"]),
            r("v2", false, &["App-win.zip"]),
            r("v1", false, &["App-win.zip"]),
        ];
        let mut p = crate::model::Config::fresh(PathBuf::new());
        p.add("a".into(), String::new(), false, false);
        let mut p = p.projects.remove(0);
        p.platforms = vec![Platform::Windows.key().to_string()];
        let tags = |n| stables(&p, &list, n).iter().map(|r| r.tag.clone()).collect::<Vec<_>>();
        assert_eq!(tags(1), ["v3"]);
        assert_eq!(tags(2), ["v3", "v2"]);
        assert_eq!(tags(10), ["v3", "v2", "v1"]);
        p.keep = 0;
        assert_eq!(p.keep(), 1);
        p.keep = 200;
        assert_eq!(p.keep(), 10);
    }

    /// Tests that set `SALVAGE_CONFIG` take this, so they never see each
    /// other's; the other tests never read it.
    static CONFIG_ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn loose_newest() {
        let _env = CONFIG_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("salvage-loose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        let dir = base.join("rel");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("SALVAGE_CONFIG", base.join("cfg").join("config.json"));
        let bus = Bus { tx: std::sync::mpsc::channel().0, ctx: eframe::egui::Context::default(), cancel: Default::default() };

        // Each release has a file whose name keeps its version and one whose name does not.
        let rel = |tag: &str| {
            let files = [format!("App-{tag}-win.zip"), "App-win.exe".to_string()];
            let assets = files
                .iter()
                .map(|name| {
                    let path = src.join(tag).join(name);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, format!("{tag} {name}")).unwrap();
                    let url = format!("file:///{}", path.display().to_string().replace('\\', "/").trim_start_matches('/'));
                    Asset { name: name.clone(), url, size: std::fs::metadata(&path).ok().map(|m| m.len()), sha256: None }
                })
                .collect();
            Release { tag: tag.into(), pre: false, date: String::new(), assets }
        };
        let mut cfg = crate::model::Config::fresh(base.clone());
        cfg.add("a".into(), String::new(), false, false);
        let mut p = cfg.projects.remove(0);
        p.releases = true;
        p.platforms = vec![ALL.to_string()];
        p.release_dir = Some(dir.clone());
        p.flat = true;
        p.keep = 2;
        let read = |path: PathBuf| std::fs::read_to_string(path).unwrap_or_default();
        // The user's own file, in the folder with the release: never touched.
        std::fs::write(dir.join("save.dat"), "mine").unwrap();

        let v1 = rel("v1");
        archive(&bus, &p, &base, std::slice::from_ref(&v1)).unwrap();
        assert_eq!(read(dir.join("App-win.exe")), "v1 App-win.exe");
        assert_eq!(read(dir.join("App-v1-win.zip")), "v1 App-v1-win.zip");
        assert!(!dir.join("v1").exists());

        // A new release: v1 goes to OLD/v1/, v2 takes its place.
        let v2 = rel("v2");
        archive(&bus, &p, &base, &[rel("v2"), rel("v1")]).unwrap();
        assert_eq!(read(dir.join("App-win.exe")), "v2 App-win.exe");
        assert!(dir.join("App-v2-win.zip").is_file());
        assert!(!dir.join("App-v1-win.zip").exists());
        assert_eq!(read(dir.join("OLD").join("v1").join("App-win.exe")), "v1 App-win.exe");
        assert_eq!(read(dir.join("save.dat")), "mine");

        // Keeping one: OLD goes.
        p.keep = 1;
        archive(&bus, &p, &base, std::slice::from_ref(&v2)).unwrap();
        assert!(!dir.join("OLD").exists());
        assert!(dir.join("App-win.exe").is_file());

        // A file of the user's with the name of a new release file: nothing moves.
        let v3 = rel("v3");
        std::fs::write(dir.join("App-v3-win.zip"), "mine too").unwrap();
        assert!(archive(&bus, &p, &base, std::slice::from_ref(&v3)).is_err());
        assert_eq!(read(dir.join("App-win.exe")), "v2 App-win.exe");
        assert_eq!(read(dir.join("App-v3-win.zip")), "mine too");
        std::fs::remove_file(dir.join("App-v3-win.zip")).unwrap();
        archive(&bus, &p, &base, std::slice::from_ref(&v3)).unwrap();
        assert_eq!(read(dir.join("App-win.exe")), "v3 App-win.exe");
        assert!(!dir.join("v3").exists());

        // Turned off: the loose files go back into v3/.
        p.flat = false;
        archive(&bus, &p, &base, std::slice::from_ref(&v3)).unwrap();
        assert!(!dir.join("App-win.exe").exists());
        assert_eq!(read(dir.join("v3").join("App-win.exe")), "v3 App-win.exe");
        assert_eq!(read(dir.join("save.dat")), "mine");
        assert!(read_loose(&loose_path()).is_empty());

        std::env::remove_var("SALVAGE_CONFIG");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn owned_list() {
        let _env = CONFIG_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("salvage-owned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("rel");
        std::fs::create_dir_all(dir.join("OLD")).unwrap();
        std::env::set_var("SALVAGE_CONFIG", base.join("cfg").join("config.json"));
        let bus = Bus { tx: std::sync::mpsc::channel().0, ctx: eframe::egui::Context::default(), cancel: Default::default() };

        // An older Salvage's list in the folder moves into the central file.
        std::fs::write(dir.join(OLD_OWNED_FILE), r#"["v1"]"#).unwrap();
        let mut owned = Owned::load(&bus, "t", &dir, false);
        assert!(!dir.join(OLD_OWNED_FILE).exists());
        assert!(owned.has(&dir.join("v1")));
        assert!(!owned.has(&dir.join("mine")));

        owned.forget(&dir.join("v1"));
        owned.add(&dir.join("OLD").join("v1"));
        owned.save().unwrap();
        let again = Owned::load(&bus, "t", &dir, false);
        assert_eq!(again.list, ["OLD/v1"]);
        let central = read_owned(&owned_path());
        assert_eq!(central.len(), 1);
        assert!(owned_path().starts_with(base.join("cfg")));

        std::env::remove_var("SALVAGE_CONFIG");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn progress() {
        assert!(crate::git::parse_progress("######                  12.5%").is_some_and(|(_, p)| (p - 0.125).abs() < 1e-4));
        assert!(crate::git::parse_progress("Receiving objects:  45% (450/1000)").is_some_and(|(ph, p)| ph == "Receiving objects" && (p - 0.45).abs() < 1e-4));
        assert!(crate::git::parse_progress("hello 50%").is_none());
    }
}
