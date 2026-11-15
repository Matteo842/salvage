//! Files attached to a repo's releases (builds, installers), read from the
//! host's web API and downloaded with `curl`, which ships with Windows 10 and 11.
//!
//! Only the newest release is kept, in `Name-releases/latest/<tag>/`, and the
//! newest pre-release when it is newer than that, in `Name-releases/prerelease/<tag>/`.
//! A new release is downloaded and checked (sha256 when the host publishes it,
//! size otherwise) before the old one is deleted.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::git::{curl, stopped_msg, Bus, Event, LogKind};
use crate::lang::tr;
use crate::model::{sanitize_name, Project};

/// Saved in a project's platforms: every file, whatever system it is for.
pub const ALL: &str = "all";

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
    if e.contains("error: 404") {
        tr("releases not found (repo removed, made private, or the host has no releases)", "release non trovate (repo rimosso, reso privato, o l'host non ha release)").into()
    } else if e.contains("error: 403") || e.contains("error: 429") {
        tr("the host refused: too many requests, try again in an hour", "l'host rifiuta: troppe richieste, riprova tra un'ora").into()
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
        let done = curl(bus, tag, None, &["-fsSL", "-H", "Accept: application/json", "-A", "Salvage", &api_url(api, &base, &path)], Some(Duration::from_secs(40)));
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
        let mut out: Vec<Choice> = Platform::ALL.iter().map(|&platform| Choice { platform, files: Vec::new() }).collect();
        let mut seen: Vec<String> = Vec::new();
        for r in [stable, pre].into_iter().flatten() {
            for a in &r.assets {
                let Some(pl) = platform(&a.name) else {
                    continue;
                };
                // The pre-release often has the same files under a newer version.
                let key = file_key(&r.tag, &a.name);
                if !seen.contains(&key) {
                    seen.push(key);
                    if let Some(c) = out.iter_mut().find(|c| c.platform == pl) {
                        c.files.push(a.name.clone());
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

fn remove_all(bus: &Bus, tag: &str, dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => bus.log(tag, LogKind::Info, t!("removed {}", "rimossa {}", dir.display())),
        Err(e) => bus.log(tag, LogKind::Err, t!("cannot remove {}: {e}", "non riesco a rimuovere {}: {e}", dir.display())),
    }
}

/// Bring one slot (`latest` or `prerelease`) to the chosen files of `rel`.
fn sync_slot(bus: &Bus, p: &Project, slot: &Path, rel: &Release) -> Result<(), String> {
    let tag = p.name.as_str();
    let wanted: Vec<&Asset> = rel
        .assets
        .iter()
        .filter(|a| platform(&a.name).is_some_and(|pl| p.platforms.iter().any(|k| k == ALL || k == pl.key())))
        .collect();
    if wanted.is_empty() {
        bus.log(tag, LogKind::Info, t!("{}: none of the chosen files is in this release", "{}: nessuno dei file scelti è in questa release", rel.tag));
        return Ok(());
    }
    let current = slot.join(sanitize_name(&rel.tag));
    let mut sums = None;

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
        // New release: everything goes into a side folder first. The old
        // release is deleted only once every new file has passed its check.
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
                bus.log(tag, LogKind::Err, t!("{e}; the old release stays, the next sync picks up from here", "{e}; la release vecchia resta, la prossima sincronizzazione riprende da qui"));
                return Err(e);
            }
        }
        std::fs::rename(&incoming, &current).map_err(|e| t!("cannot move {}: {e}", "non riesco a spostare {}: {e}", incoming.display()))?;
    }
    for old in subdirs(slot).into_iter().filter(|d| *d != current) {
        remove_all(bus, tag, &old);
    }
    Ok(())
}

/// Keep `latest/` and `prerelease/` on the newest releases, with the chosen files.
pub fn sync(bus: &Bus, p: &Project, root: &Path) -> Result<(), String> {
    let tag = p.name.as_str();
    let releases = fetch(bus, tag, &p.url).inspect_err(|e| bus.log(tag, LogKind::Err, t!("releases: {e}", "release: {e}")))?;
    let (stable, pre) = pick(&releases);
    let dir = p.releases_dir(root);
    let mut first_err = None;

    // No stable release upstream (deleted, or never made): keep what is on disk.
    if let Some(rel) = stable {
        if let Err(e) = sync_slot(bus, p, &dir.join("latest"), rel) {
            first_err.get_or_insert(e);
        }
    }
    match pre {
        Some(rel) => {
            if let Err(e) = sync_slot(bus, p, &dir.join("prerelease"), rel) {
                first_err.get_or_insert(e);
            }
        }
        // A stable release came out after the last pre-release: that one is outdated.
        None if stable.is_some() => {
            for old in subdirs(&dir.join("prerelease")) {
                remove_all(bus, tag, &old);
            }
        }
        None => {}
    }
    match first_err {
        Some(e) => Err(e),
        None => {
            let names = |r: Option<&Release>| r.map_or("-".to_string(), |r| r.tag.clone());
            bus.log(tag, LogKind::Good, t!("releases: latest {}, pre-release {}", "release: latest {}, pre-release {}", names(stable), names(pre)));
            Ok(())
        }
    }
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
    fn progress() {
        assert!(crate::git::parse_progress("######                  12.5%").is_some_and(|(_, p)| (p - 0.125).abs() < 1e-4));
        assert!(crate::git::parse_progress("Receiving objects:  45% (450/1000)").is_some_and(|(ph, p)| ph == "Receiving objects" && (p - 0.45).abs() < 1e-4));
        assert!(crate::git::parse_progress("hello 50%").is_none());
    }
}
