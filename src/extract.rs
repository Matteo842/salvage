//! Unpacking release archives for the auto-updater (zip, 7z, tar.gz, tar.xz),
//! all in-process: nothing to install next to the exe.
//!
//! An archive is first unpacked into an empty staging folder, then its files
//! are moved over the folder being updated. Entries that would land outside
//! the staging folder (`..`, absolute paths, drive letters) and links are
//! skipped. Files that are only in the updated folder (saves, settings) stay.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use crate::lang::tr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Zip,
    SevenZ,
    TarGz,
    TarXz,
}

/// The archive format, from the file name. `None` for anything else (an
/// installer, an AppImage, a lone exe): the updater copies those as they are.
pub fn kind(name: &str) -> Option<Kind> {
    let n = name.to_lowercase();
    if n.ends_with(".zip") {
        Some(Kind::Zip)
    } else if n.ends_with(".7z") {
        Some(Kind::SevenZ)
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(Kind::TarGz)
    } else if n.ends_with(".tar.xz") || n.ends_with(".txz") {
        Some(Kind::TarXz)
    } else {
        None
    }
}

/// An entry name made into a path under the staging folder, or `None` when it
/// would escape it or names nothing.
fn safe_rel(name: &str) -> Option<PathBuf> {
    let name = name.replace('\\', "/");
    if name.starts_with('/') || name.contains(':') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in Path::new(&name).components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn write_file(into: &Path, rel: &Path, reader: &mut dyn Read, mode: Option<u32>) -> Result<(), String> {
    let path = into.join(rel);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut f = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::io::copy(reader, &mut f).map_err(|e| format!("{}: {e}", rel.display()))?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o777));
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

fn unpack_tar(reader: impl Read, into: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = entry.path().map_err(|e| e.to_string())?.to_string_lossy().to_string();
        let Some(rel) = safe_rel(&name) else {
            continue;
        };
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(into.join(&rel)).map_err(|e| e.to_string())?;
        } else if kind.is_file() {
            let mode = entry.header().mode().ok();
            write_file(into, &rel, &mut entry, mode)?;
        }
        // Links and devices are skipped: a link could point anywhere.
    }
    Ok(())
}

fn unpack_zip(file: File, into: &Path) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().map_err(|e| e.to_string())?.to_string();
        let Some(rel) = safe_rel(&name) else {
            continue;
        };
        if f.is_dir() {
            std::fs::create_dir_all(into.join(&rel)).map_err(|e| e.to_string())?;
        } else if !f.is_symlink() {
            let mode = f.unix_mode();
            write_file(into, &rel, &mut f, mode)?;
        }
    }
    Ok(())
}

fn unpack_7z(archive: &Path, into: &Path) -> Result<(), String> {
    let mut failed = None;
    sevenz_rust2::decompress_file_with_extract_fn(archive, into, |entry, reader, _| {
        match safe_rel(entry.name()) {
            Some(rel) if entry.is_directory() => {
                std::fs::create_dir_all(into.join(&rel))?;
            }
            Some(rel) => {
                if let Err(e) = write_file(into, &rel, reader, None) {
                    failed = Some(e);
                    return Ok(false);
                }
            }
            // Read it anyway: in a solid archive the next file follows this one.
            None => {
                std::io::copy(reader, &mut std::io::sink())?;
            }
        }
        Ok(true)
    })
    .map_err(|e| e.to_string())?;
    failed.map_or(Ok(()), Err)
}

/// Unpack `archive` into `into`, which is emptied first.
pub fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let name = archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let Some(kind) = kind(&name) else {
        return Err(t!("{name}: not an archive", "{name}: non è un archivio"));
    };
    if into.exists() {
        std::fs::remove_dir_all(into).map_err(|e| format!("{}: {e}", into.display()))?;
    }
    std::fs::create_dir_all(into).map_err(|e| format!("{}: {e}", into.display()))?;
    let open = || File::open(archive).map_err(|e| format!("{name}: {e}"));
    let result = match kind {
        Kind::Zip => unpack_zip(open()?, into),
        Kind::SevenZ => unpack_7z(archive, into),
        Kind::TarGz => unpack_tar(flate2::read::GzDecoder::new(BufReader::new(open()?)), into),
        Kind::TarXz => unpack_tar(lzma_rust2::XzReader::new(BufReader::new(open()?), true), into),
    };
    result.map_err(|e| t!("{name}: cannot unpack: {e}", "{name}: non riesco a estrarre: {e}"))
}

/// Many archives wrap everything in one folder (`App-1.2/app.exe`). The
/// updater wants what is inside it, so `App-1.2/` is looked through.
pub fn content_root(dir: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    match entries.as_slice() {
        [only] if only.is_dir() => only.clone(),
        _ => dir.to_path_buf(),
    }
}

/// Every file under `dir`, as paths relative to it.
fn files(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = e.path();
        if path.is_dir() {
            files(&path, base, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(rel.to_path_buf());
        }
    }
}

/// Can `path` be replaced right now? A program that is running keeps its exe
/// and libraries locked on Windows.
fn replaceable(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        return Err(t!("{} is a folder in the way of a file", "{} è una cartella al posto di un file", path.display()));
    }
    if path.exists() {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| t!("{} is in use ({e}): close the program and sync again", "{} è in uso ({e}): chiudi il programma e sincronizza di nuovo", path.display()))?;
    }
    Ok(())
}

/// Move every file of `from` over `to`, replacing the ones with the same name.
/// Nothing is touched unless every file can be replaced. Returns how many
/// files were put in place.
pub fn install(from: &Path, to: &Path) -> Result<usize, String> {
    let mut list = Vec::new();
    files(from, from, &mut list);
    if list.is_empty() {
        return Err(tr("the archive is empty", "l'archivio è vuoto").into());
    }
    for rel in &list {
        replaceable(&to.join(rel))?;
    }
    for rel in &list {
        let (src, dst) = (from.join(rel), to.join(rel));
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        if std::fs::rename(&src, &dst).is_err() {
            std::fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        }
    }
    Ok(list.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn kinds() {
        assert_eq!(kind("Eden-Windows.ZIP"), Some(Kind::Zip));
        assert_eq!(kind("a.tar.gz"), Some(Kind::TarGz));
        assert_eq!(kind("a.tar.xz"), Some(Kind::TarXz));
        assert_eq!(kind("a.7z"), Some(Kind::SevenZ));
        assert_eq!(kind("App.AppImage"), None);
        assert_eq!(kind("setup.exe"), None);
    }

    #[test]
    fn paths() {
        assert_eq!(safe_rel("a/b.txt"), Some(PathBuf::from("a").join("b.txt")));
        assert_eq!(safe_rel("./a"), Some(PathBuf::from("a")));
        assert_eq!(safe_rel("a\\..\\..\\x"), None);
        assert_eq!(safe_rel("../x"), None);
        assert_eq!(safe_rel("/etc/x"), None);
        assert_eq!(safe_rel("C:/x"), None);
        assert_eq!(safe_rel("a/"), Some(PathBuf::from("a")));
        assert_eq!(safe_rel(""), None);
    }

    #[test]
    fn zip_over_folder() {
        let base = std::env::temp_dir().join(format!("salvage-extract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let archive = base.join("App-1.2.zip");
        {
            let mut z = zip::ZipWriter::new(File::create(&archive).unwrap());
            let opt = zip::write::SimpleFileOptions::default();
            z.start_file("App-1.2/app.exe", opt).unwrap();
            z.write_all(b"new").unwrap();
            z.start_file("App-1.2/lib/core.dll", opt).unwrap();
            z.write_all(b"core").unwrap();
            z.start_file("../evil.txt", opt).unwrap();
            z.write_all(b"x").unwrap();
            z.finish().unwrap();
        }
        let target = base.join("App");
        std::fs::create_dir_all(target.join("user")).unwrap();
        std::fs::write(target.join("app.exe"), b"old").unwrap();
        std::fs::write(target.join("user").join("save.dat"), b"mine").unwrap();

        let staging = base.join("staging");
        unpack(&archive, &staging).unwrap();
        assert!(!base.join("evil.txt").exists());
        let n = install(&content_root(&staging), &target).unwrap();
        assert_eq!(n, 2);
        assert_eq!(std::fs::read(target.join("app.exe")).unwrap(), b"new");
        assert_eq!(std::fs::read(target.join("lib").join("core.dll")).unwrap(), b"core");
        assert_eq!(std::fs::read(target.join("user").join("save.dat")).unwrap(), b"mine");
        let _ = std::fs::remove_dir_all(&base);
    }
}
