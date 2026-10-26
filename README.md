# Craft Updater

[![CI](https://github.com/hellproducts/CraftUpdater/actions/workflows/ci.yml/badge.svg)](https://github.com/hellproducts/CraftUpdater/actions/workflows/ci.yml)

A tiny **native Windows tray app**, written in Rust with [egui](https://github.com/emilk/egui),
that keeps the **portable** editions of the [ArtCraft](https://getartcraft.com)
apps — PhotoCraft, VectorCraft and the rest — up to date from their official
GitHub Releases.

Add only the apps you use, point each at its portable folder, and let it handle
the rest: arch-aware downloads, checksum verification, backups, scheduled checks
and update pop-ups. One `.exe`, no installer, no runtime.

---

## Features

- **Modular** — starts empty; add the apps you want from a dropdown and remove them any time.
- **Right download, every time** — picks the matching `windows-<arch>-portable.zip` for your CPU (x64 / arm64 / x86).
- **Safe updates** — verifies size and SHA-256 (when published), backs up the current version, overlays the new files, and rolls back if anything fails.
- **Keeps your data** — settings, presets and your own files survive updates; it updates the program files only.
- **Backups & restore** — keeps the last *N* versions and restores any with one click.
- **Launch buttons** — one per executable in each app's folder (e.g. *Launch PhotoCraft*, *Launch PhotoCraft CLI*).
- **Lives in the tray** — minimizes to the notification area with no taskbar button; scheduled checks (hourly … weekly) and a pop-up when an update is found.
- **Quality-of-life** — remembers its window size, optional *start minimized* and *launch at sign-in*, and a choice of whether the **X** button hides to tray or quits.

Settings live in `craft-updater.toml` next to the `.exe`, so the whole thing stays portable.

---

## Install

Grab `craft-updater.exe` from the [Releases](../../releases) page (or build it
yourself — see below) and drop it wherever you like, e.g. next to your
PortableApps folders.

**First run:** the app is empty. Click **Add app**, choose a product, and pick
its portable folder when prompted. Add as many or as few as you use.

> `Installed: —` is normal before the first update — the Rust-built `.exe`
> carries no version info. After one update the app records the version in a
> small `.craft-version` marker in the program folder.

---

## Building

You need the Rust toolchain ([rustup.rs](https://rustup.rs)).

### Native Windows (MSVC)

```powershell
cargo build --release
# → target\release\craft-updater.exe
```

If prompted, install the **Visual Studio C++ Build Tools** (the rustup
installer links to them).

### From WSL (cross-compile to Windows)

A plain `cargo build` in WSL makes a *Linux* binary. To produce a Windows
`.exe`, cross-compile to the GNU target:

```bash
sudo apt update && sudo apt install -y mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
# → target/x86_64-pc-windows-gnu/release/craft-updater.exe
```

Uses `rustls` for TLS (no OpenSSL), so the only C dependency is the MinGW
toolchain above. The tray, toast and registry code uses the `windows` crate
bindings, which cross-compile cleanly on the GNU target.

### ARM64 Windows

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

Run the logic tests any time with `cargo test`.

---

## How it works

1. Queries `api.github.com/repos/<owner>/<repo>/releases/latest` for each configured app.
2. Selects the `windows-<arch>-portable.zip` asset for your CPU.
3. Compares it with the installed version (from the `.craft-version` marker).
4. On update: downloads to a temp folder, verifies the checksum, backs up the
   current program folder to `<App>-backups\`, extracts, and overlays the new files.
5. Writes the new version to `.craft-version`.

It locates the real program folder automatically, including the PortableApps.com
`XxxPortable\App\Xxx\` layout.

The app only talks to `api.github.com` and the GitHub download URLs. Set a
`GITHUB_TOKEN` environment variable if you ever hit GitHub's unauthenticated
rate limit (optional).

---

## Supported apps

The **Add** dropdown lists the ArtCraft family. **PhotoCraft** and
**VectorCraft** point at confirmed repositories; the others
(LightCraft, FilmCraft, EffectCraft, PrintCraft, DesignCraft, PdfCraft) follow
the same `storytold/<name>` pattern. Any app that publishes a
`...windows-<arch>-portable.zip` asset on GitHub Releases works.

---

## Project layout

```
Cargo.toml       dependencies & release profile
build.rs         embeds the app icon into the .exe (Explorer icon)
app.rc           Windows resource script for the icon
src/core.rs      updater logic (config, GitHub, versions, install, backup) + tests
src/main.rs      egui GUI, tray, scheduler, toasts, theme
assets/          app icon (png + ico) and tray icon
```

---

## Tech

Rust · egui/eframe · reqwest (rustls) · zip · sha2 · tray-icon ·
tauri-winrt-notification · winreg · windows-sys

## License

MIT OR Apache-2.0 — your choice. *(Add `LICENSE` file(s) to confirm.)*
