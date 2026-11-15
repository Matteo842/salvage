//! Finding Git, and getting it on a Windows PC that has none.
//!
//! Git is looked for on the PATH, then in Salvage's own `git` folder (MinGit,
//! see [`install_mingit`]), then where the Git for Windows installer puts it.
//! The last two are why no restart is needed after an install: a running
//! program, and anything it starts again, keeps the PATH it was born with.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::git::{self, Bus, LogKind};
use crate::lang::tr;

/// Tag the install steps show up under in the terminal.
pub fn tag() -> &'static str {
    tr("git setup", "installa git")
}

/// `None`: not looked for yet. `Some(None)`: looked for, not there.
static FOUND: Mutex<Option<Option<PathBuf>>> = Mutex::new(None);

/// The `git` to run, or `None` if this PC has none.
pub fn git() -> Option<PathBuf> {
    let mut found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    found.get_or_insert_with(find).clone()
}

/// Look again on the next [`git`] call: after an install.
pub fn forget() {
    *FOUND.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Where MinGit goes: next to Salvage's settings (the exe folder when portable).
pub fn mingit_dir() -> PathBuf {
    crate::model::config_path().with_file_name("git")
}

fn find() -> Option<PathBuf> {
    let mut places = vec![PathBuf::from("git")];
    if cfg!(windows) {
        places.push(mingit_dir().join("cmd").join("git.exe"));
        for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(var) {
                places.push(Path::new(&dir).join("Git").join("cmd").join("git.exe"));
            }
        }
        // Git for Windows installed for one user only.
        if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
            places.push(Path::new(&dir).join("Programs").join("Git").join("cmd").join("git.exe"));
        }
    }
    places.into_iter().find(|p| (p.components().count() == 1 || p.is_file()) && answers(p))
}

fn answers(exe: &Path) -> bool {
    let mut c = Command::new(exe);
    c.arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c.status().is_ok_and(|s| s.success())
}

/// After an install: Salvage can run Git now, or why not.
fn found_after(bus: &Bus, how: &str) -> Result<(), String> {
    forget();
    let exe = git().ok_or_else(|| t!("{how} finished, but Salvage still cannot find git.exe", "{how} è finito, ma Salvage non trova ancora git.exe"))?;
    bus.log(tag(), LogKind::Good, t!("git ready: {}", "git pronto: {}", exe.display()));
    Ok(())
}

/// Git for Windows, for the whole PC, through winget. Windows asks for admin
/// rights. The installer adds Git to the system PATH.
pub fn install_system(bus: &Bus) -> Result<(), String> {
    let args = [
        "install",
        "--id",
        "Git.Git",
        "--exact",
        "--source",
        "winget",
        "--scope",
        "machine",
        "--silent",
        "--accept-package-agreements",
        "--accept-source-agreements",
        "--disable-interactivity",
    ];
    let done = git::program(bus, tag(), "winget", &args, None);
    if done.ok {
        return found_after(bus, "winget");
    }
    // winget also fails when Git is already there: it may just be off the PATH.
    if found_after(bus, "winget").is_ok() {
        return Ok(());
    }
    Err(if done.err == git::missing_msg("winget") {
        tr(
            "winget is not on this PC: use the portable MinGit, or get Git from git-scm.com",
            "winget non c'è su questo PC: usa MinGit portatile, o scarica Git da git-scm.com",
        )
        .to_string()
    } else if done.err.trim().is_empty() {
        tr("the Git installer failed or was cancelled", "l'installazione di Git è fallita o è stata annullata").to_string()
    } else {
        done.err.trim().to_string()
    })
}

/// MinGit: the official minimal Git for Windows, unpacked into Salvage's own
/// `git` folder. No admin rights, nothing installed on the system.
pub fn install_mingit(bus: &Bus) -> Result<(), String> {
    let tag = tag();
    let api = "https://api.github.com/repos/git-for-windows/git/releases/latest";
    let done = git::curl(bus, tag, None, &["-fsSL", "-H", "Accept: application/vnd.github+json", "-A", "Salvage", api], Some(Duration::from_secs(40)));
    if !done.ok {
        return Err(t!("cannot read the Git for Windows releases: {}", "non riesco a leggere le release di Git for Windows: {}", done.err.trim()));
    }
    let release: Value = serde_json::from_str(&done.stdout).map_err(|e| e.to_string())?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "64-bit",
        "aarch64" => "arm64",
        _ => "32-bit",
    };
    let suffix = format!("-{arch}.zip");
    let asset = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.starts_with("MinGit-") && n.ends_with(&suffix) && !n.contains("busybox")))
        .ok_or_else(|| t!("the latest Git for Windows release has no MinGit for {arch}", "l'ultima release di Git for Windows non ha MinGit per {arch}"))?;
    let name = asset["name"].as_str().unwrap_or_default();
    let url = asset["browser_download_url"].as_str().unwrap_or_default();
    let size = asset["size"].as_u64();
    let sha256 = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase);

    let dir = mingit_dir();
    let parent = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let part = parent.join("MinGit.zip.part");
    let zip = parent.join("MinGit.zip");
    bus.log(tag, LogKind::Info, name.to_string());
    let target = part.to_string_lossy().to_string();
    let done = git::curl(bus, tag, None, &["-fL", "-#", "--retry", "2", "-A", "Salvage", "-o", &target, url], None);
    if !done.ok {
        let _ = std::fs::remove_file(&part);
        return Err(t!("MinGit download failed: {}", "download di MinGit fallito: {}", done.err.trim()));
    }

    let bad = |why: String| {
        let _ = std::fs::remove_file(&part);
        Err(format!("{name}: {why}"))
    };
    if let Some(want) = sha256 {
        let got = sha256_file(&part).map_err(|e| e.to_string())?;
        if got != want {
            return bad(tr("sha256 does not match, file discarded", "lo sha256 non corrisponde, file scartato").into());
        }
    } else if let Some(size) = size {
        let len = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if len != size {
            return bad(t!("{len} bytes instead of {size}, file discarded", "{len} byte invece di {size}, file scartato"));
        }
    }
    std::fs::rename(&part, &zip).map_err(|e| format!("{name}: {e}"))?;
    let unpacked = crate::extract::unpack(&zip, &dir);
    let _ = std::fs::remove_file(&zip);
    unpacked?;
    bus.log(tag, LogKind::Info, t!("MinGit unpacked in {}", "MinGit estratto in {}", dir.display()));
    found_after(bus, "MinGit")
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
