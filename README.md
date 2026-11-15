# Salvage

Keeps local backups of git repos that might vanish.

Give it a list of git links. For each one it keeps a full mirror (every branch
and tag) and a normal working copy with submodules, and it keeps both up to
date. When a repo is taken down or moves to another host, you still have
everything, and you can point the backup at the new link in two clicks.

![Salvage syncing four repos](docs/screenshot.png)

## What it does

- **Any git host.** GitHub, GitLab, Gitea/Forgejo, a self-hosted server: if
  `git` can clone it, Salvage can back it up.
- **Tells you what is still alive.** At startup it asks every link whether it
  still answers. Green means up to date, orange means there is something to
  download, a red skull means the link is dead (your backup stays on disk).
- **Syncs on a timer.** Every N hours while the window is open, or while it is
  closed through the Windows Task Scheduler (a systemd user timer on Linux).
  No service, nothing left running: the system starts Salvage with no window,
  it syncs, it quits.
- **Keeps the builds too, if you want them.** Pick a system from the release
  files menu: it lists only the ones the newest release has builds for
  (Windows, Linux, Android, macOS), or all of them. Salvage tells which file is
  for which system from its name and extension.
  Salvage keeps the newest release (or the newest 2 to 10, you choose: the
  older ones go in `OLD/`) and the newest pre-release. When a new one comes out
  it is downloaded and checked against its sha256 (or its size, when the host
  publishes no hash) before the oldest one is deleted. Each repo can put its
  releases in a folder of its own instead of the backup folder; in there
  Salvage only ever moves or deletes the folders it made, which it lists in
  `releases.json` next to its config (not in the folder itself). Works with GitHub,
  GitLab and Gitea/Forgejo. You can keep only the releases, without mirror or
  working copy.
- **Keeps a program up to date.** Pick one release file and a folder: at every
  sync the newest build is unpacked over it (zip, 7z, tar.gz, tar.xz; any other
  file is copied as it is). Files that are only in the folder, like saves and
  settings, stay. If the program is running, nothing is touched and the update
  waits for the next sync. Stable releases only, or pre-releases too.
- **Follows a repo that moves.** Click the pencil, paste the new link. The
  folders on disk stay the same and the old link is kept in a history.
- **Never deletes your backups.** Removing a repo from the list leaves its files.
  Re-cloning a working copy renames the old one to `Name.old-<date>`. The only
  thing ever deleted is an old release, once its replacement has passed the check.
- **Two looks.** The deep-water theme, or OLED: pure black and white. Switch
  in the settings.
- **Shows its work.** Every git command and its output scrolls in the terminal
  behind the interface. Press `F2` to see it full screen.

On disk, under the backup folder you choose:

```
Name-mirror.git/   bare mirror: all branches, all tags
Name/              working copy, submodules included
Name-releases/       (or the folder picked for the repo)
  <tag>/             files of the newest release
  OLD/<tag>/         the releases before it, if you keep more than one
  prerelease/<tag>/  files of the newest pre-release, if newer than those
```

Repos already in that folder are found and added on first run.

## Requirements

- Windows 10 or 11, or Linux x86_64 (glibc 2.31 or newer: Ubuntu 20.04,
  Debian 11, Fedora, Arch and the like), on X11 or Wayland
- [`git`](https://git-scm.com/downloads), for repos with a mirror or a working
  copy. Repos kept only for their release files work without it. On Windows,
  if Git is missing, Salvage offers to install it: for the whole PC through
  `winget` (asks for admin rights), or as portable MinGit in Salvage's own
  `git` folder (next to the exe in portable mode). Git in `Program Files` is
  found even when it is not on the `PATH`.
- `curl` for release files (it comes with Windows 10 and 11, and with nearly
  every Linux distro)

## Download

From the [latest release](https://github.com/Matteo842/salvage/releases/latest):

- **Windows:** `Salvage.exe`. It is a single file, no installer. The exe is not
  signed, so Windows SmartScreen may ask for confirmation the first time.
- **Linux:** `salvage-<version>-linux-x86_64.tar.gz`. Unpack it and run
  `./salvage`, or run `./install.sh` to put it in `~/.local/bin` with an entry
  in your app menu (`./install.sh --uninstall` removes it).

## Build and run

```
cargo build --release
target\release\salvage.exe --root "D:\backup"
```

`--root` is only needed the first time (you can also change the folder in the
settings). The list is stored in `%APPDATA%\Salvage\config.json`, the log of
the last scheduled run in `last-sync.log` and the release folders Salvage made
in `releases.json`, all in that same folder. On Linux they live in
`~/.config/salvage/`, and the default backup folder is `~/Salvage`.

### Portable mode

Put an empty file named `portable` (or `portable.txt`) next to the executable,
and Salvage keeps all of its own files in that folder instead: `config.json`,
`releases.json`, `last-sync.log` and its lock file. Nothing goes in
`%APPDATA%\Salvage` or `~/.config/salvage`, so the folder can live on a USB
stick or be copied to another computer (only the scheduled sync, if you turn it
on, is still registered with the system: Task Scheduler, or a systemd user
timer). The backups themselves still go in the backup folder you
choose. To carry over an existing setup, copy `config.json` and `releases.json`
from `%APPDATA%\Salvage` (or `~/.config/salvage`) next to the executable.
The scheduled sync runs the same executable, so it finds the same files.

The Linux tarball is built with `packaging/linux/build.sh` (on Linux or WSL,
with `rustup`, `zig` and `cargo-zigbuild`); it links against glibc 2.31 so the
binary runs on older distros too.

The interface is in English, or in Italian on an Italian system. Set
`SALVAGE_LANG=en` or `SALVAGE_LANG=it` to force one.

## Built with

Rust and [egui](https://github.com/emilk/egui). The icon, the sonar and every
control are drawn in code; there are no image assets.

## License

[MIT](LICENSE)
