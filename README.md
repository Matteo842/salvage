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
  Salvage keeps the newest release and the newest pre-release. When a new one
  comes out it is downloaded and checked against its sha256 (or its size, when
  the host publishes no hash) before the old one is deleted. Works with GitHub,
  GitLab and Gitea/Forgejo. You can keep only the releases, without mirror or
  working copy.
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
Name-releases/
  latest/<tag>/      files of the newest release
  prerelease/<tag>/  files of the newest pre-release, if newer than that
```

Repos already in that folder are found and added on first run.

## Requirements

- Windows 10 or 11, or Linux x86_64 (glibc 2.31 or newer: Ubuntu 20.04,
  Debian 11, Fedora, Arch and the like), on X11 or Wayland
- [`git`](https://git-scm.com/downloads) in your `PATH`
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
settings). The list is stored in `%APPDATA%\Salvage\config.json`, and the log
of the last scheduled run in `%APPDATA%\Salvage\last-sync.log`. On Linux both
live in `~/.config/salvage/`, and the default backup folder is `~/Salvage`.

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
