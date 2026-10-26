//! Platform-independent updater logic: config, GitHub release lookup,
//! version parsing, download/verify/install, backup and restore.
//! Everything here is testable on any OS.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Marker file written into an app folder after a successful install, so we can
/// tell which version is installed even when the .exe carries no version info.
pub const VERSION_MARKER: &str = ".craft-version";

// ---------------------------------------------------------------- configuration

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CheckInterval {
    Manual,
    OnStartup,
    Hourly,
    EverySixHours,
    Daily,
    Weekly,
}

impl Default for CheckInterval {
    fn default() -> Self {
        CheckInterval::Daily
    }
}

impl CheckInterval {
    /// Background poll period. `None` means "never poll on a timer".
    pub fn as_duration(&self) -> Option<std::time::Duration> {
        use std::time::Duration;
        match self {
            CheckInterval::Manual | CheckInterval::OnStartup => None,
            CheckInterval::Hourly => Some(Duration::from_secs(3600)),
            CheckInterval::EverySixHours => Some(Duration::from_secs(6 * 3600)),
            CheckInterval::Daily => Some(Duration::from_secs(24 * 3600)),
            CheckInterval::Weekly => Some(Duration::from_secs(7 * 24 * 3600)),
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            CheckInterval::Manual => "Manual only",
            CheckInterval::OnStartup => "On startup",
            CheckInterval::Hourly => "Every hour",
            CheckInterval::EverySixHours => "Every 6 hours",
            CheckInterval::Daily => "Every day",
            CheckInterval::Weekly => "Every week",
        }
    }
    pub const ALL: [CheckInterval; 6] = [
        CheckInterval::Manual,
        CheckInterval::OnStartup,
        CheckInterval::Hourly,
        CheckInterval::EverySixHours,
        CheckInterval::Daily,
        CheckInterval::Weekly,
    ];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppEntry {
    /// Display name, also used to match the program's .exe (e.g. "PhotoCraft").
    pub name: String,
    /// GitHub "owner/repo".
    pub repo: String,
    /// Portable install folder (the XxxPortable root is fine; we locate the
    /// real program folder underneath it).
    pub install_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub apps: Vec<AppEntry>,
    pub check_interval: CheckInterval,
    pub keep_backups: usize,
    pub start_minimized: bool,
    pub launch_at_login: bool,
    pub notify_on_update: bool,
    /// When true, the window's X button hides to the tray; when false, X quits.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default = "default_w")]
    pub win_w: f32,
    #[serde(default = "default_h")]
    pub win_h: f32,
}

fn default_true() -> bool {
    true
}
fn default_w() -> f32 {
    520.0
}
fn default_h() -> f32 {
    640.0
}

/// Known ArtCraft products offered in the "Add" dropdown: (display name, GitHub repo).
/// PhotoCraft and VectorCraft repos are confirmed; the rest follow the same
/// `storytold/<name>` pattern and can be corrected per entry if needed.
pub const CATALOG: &[(&str, &str)] = &[
    ("PhotoCraft", "storytold/photocraft"),
    ("VectorCraft", "storytold/vectorcraft"),
    ("LightCraft", "storytold/lightcraft"),
    ("FilmCraft", "storytold/filmcraft"),
    ("EffectCraft", "storytold/effectcraft"),
    ("PrintCraft", "storytold/printcraft"),
    ("DesignCraft", "storytold/designcraft"),
    ("PdfCraft", "storytold/pdfcraft"),
];

impl Default for Config {
    fn default() -> Self {
        Config {
            apps: Vec::new(),
            check_interval: CheckInterval::Daily,
            keep_backups: 3,
            start_minimized: false,
            launch_at_login: false,
            notify_on_update: true,
            close_to_tray: true,
            win_w: 520.0,
            win_h: 640.0,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Config {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)?;
        std::fs::write(path, s).with_context(|| format!("writing config to {}", path.display()))?;
        Ok(())
    }
}

// ----------------------------------------------------------------- GitHub types

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub html_url: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    /// GitHub may expose "sha256:<hex>" here.
    #[serde(default)]
    pub digest: Option<String>,
}

// ------------------------------------------------------------------- versioning

/// Parse "v0.13.1", "0.8.2", "1.2.3.4" → comparable 4-tuple. Lenient.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64, u64)> {
    let s = s.trim();
    // take the first run that looks like a dotted number, skipping a leading 'v'
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let rest = &s[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let num = &rest[..end];
    let mut parts = num.split('.').filter(|p| !p.is_empty());
    let a = parts.next()?.parse().ok()?;
    let b = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let c = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let d = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((a, b, c, d))
}

/// Is `latest` newer than `installed`? Unknown installed → treat as "update".
pub fn is_newer(latest: &str, installed: Option<&str>) -> bool {
    match (parse_version(latest), installed.and_then(parse_version)) {
        (Some(l), Some(i)) => l > i,
        (Some(_), None) => true,
        _ => true,
    }
}

// ------------------------------------------------------------- arch & asset pick

pub fn current_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "x64",
    }
}

/// Choose the "...windows-<arch>-portable.zip" asset, falling back to any
/// windows portable zip if the exact arch is missing.
pub fn pick_asset<'a>(release: &'a Release, arch: &str) -> Option<&'a Asset> {
    let want = format!("windows-{arch}-portable.zip");
    release
        .assets
        .iter()
        .find(|a| a.name.to_lowercase().ends_with(&want))
        .or_else(|| {
            release.assets.iter().find(|a| {
                let n = a.name.to_lowercase();
                n.contains("windows") && n.contains("portable") && n.ends_with(".zip")
            })
        })
}

// --------------------------------------------------------- install-dir handling

/// Find the folder that actually holds `<name>*.exe`, searching up to `depth`
/// levels under `root`. Skips the portable launcher and Data/Other folders.
/// Falls back to `root` itself.
pub fn find_program_dir(root: &Path, name: &str) -> PathBuf {
    let mut best: Option<PathBuf> = None;
    let mut best_len = usize::MAX;
    let name_lc = name.to_lowercase();
    walk(root, 0, 3, &mut |p| {
        if let Some(fname) = p.file_name().and_then(|s| s.to_str()) {
            let f = fname.to_lowercase();
            if f.starts_with(&name_lc)
                && f.ends_with(".exe")
                && !f.contains("portable")
                && !f.contains("update")
                && !f.contains("unins")
            {
                let s = p.to_string_lossy().to_lowercase();
                if !s.contains("/data/")
                    && !s.contains("\\data\\")
                    && !s.contains("/other/")
                    && !s.contains("\\other\\")
                {
                    let len = p.components().count();
                    if len < best_len {
                        best_len = len;
                        best = p.parent().map(|x| x.to_path_buf());
                    }
                }
            }
        }
    });
    best.unwrap_or_else(|| root.to_path_buf())
}

fn walk(dir: &Path, depth: usize, max: usize, f: &mut dyn FnMut(&Path)) {
    if depth > max {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, depth + 1, max, f);
        } else {
            f(&p);
        }
    }
}

/// List the `.exe` files directly in the program folder (skips uninstallers).
pub fn list_executables(program_dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(program_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("exe"))
                    .unwrap_or(false)
        })
        .filter(|p| {
            let n = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            !n.contains("unins")
        })
        .collect();
    v.sort();
    v
}

pub fn read_installed_version(program_dir: &Path) -> Option<String> {
    let marker = program_dir.join(VERSION_MARKER);
    std::fs::read_to_string(marker)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------- backups

#[derive(Debug, Clone)]
pub struct Backup {
    pub path: PathBuf,
    pub label: String,
}

pub fn backup_root(install_dir: &Path, name: &str) -> PathBuf {
    let parent = install_dir.parent().unwrap_or(install_dir);
    parent.join(format!("{name}-backups"))
}

pub fn list_backups(install_dir: &Path, name: &str) -> Vec<Backup> {
    let root = backup_root(install_dir, name);
    let mut v: Vec<Backup> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| Backup {
            label: e.file_name().to_string_lossy().to_string(),
            path: e.path(),
        })
        .collect();
    v.sort_by(|a, b| b.label.cmp(&a.label)); // newest first (timestamped names)
    v
}

/// Recursively copy the contents of `src` into `dst` (dst created if needed).
pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn prune_backups(install_dir: &Path, name: &str, keep: usize) {
    let backups = list_backups(install_dir, name);
    for b in backups.into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(&b.path);
    }
}

// --------------------------------------------------------------- checksum / zip

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{:02x}", b));
    }
    Ok(hex)
}

/// Extract `zip_path` into `dest`. If the archive wraps everything in a single
/// top-level folder, returns that folder; otherwise returns `dest`.
pub fn extract_zip(zip_path: &Path, dest: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    std::fs::create_dir_all(dest)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            continue; // skip unsafe paths (zip-slip guard)
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mut w = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut w)?;
        }
    }
    // collapse single wrapping folder
    let mut items: Vec<PathBuf> = std::fs::read_dir(dest)?
        .flatten()
        .map(|e| e.path())
        .collect();
    if items.len() == 1 && items[0].is_dir() {
        return Ok(items.remove(0));
    }
    Ok(dest.to_path_buf())
}

// ---------------------------------------------------------------- orchestration

/// Result of a version check for one app.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub installed: Option<String>,
    pub latest: String,
    pub update_available: bool,
    pub asset_name: String,
    pub asset_url: String,
    pub asset_size: u64,
    pub asset_digest: Option<String>,
    pub release_notes: String,
    pub release_url: String,
    pub program_dir: PathBuf,
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("craft-updater/1.0")
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(Into::into)
}

fn get_release(client: &reqwest::blocking::Client, repo: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut req = client.get(&url).header("Accept", "application/vnd.github+json");
    if let Ok(tok) = std::env::var("GITHUB_TOKEN") {
        if !tok.is_empty() {
            req = req.header("Authorization", format!("Bearer {tok}"));
        }
    }
    let resp = req.send().context("contacting GitHub")?;
    if !resp.status().is_success() {
        bail!("GitHub returned HTTP {} for {}", resp.status().as_u16(), repo);
    }
    resp.json::<Release>().context("parsing release JSON")
}

/// Check one app for updates (network).
pub fn check_app(app: &AppEntry, arch: &str) -> Result<CheckResult> {
    let install_dir = PathBuf::from(&app.install_dir);
    if app.install_dir.is_empty() {
        bail!("No install folder set for {}", app.name);
    }
    let program_dir = find_program_dir(&install_dir, &app.name);
    let installed = read_installed_version(&program_dir);

    let client = http_client()?;
    let release = get_release(&client, &app.repo)?;
    let asset = pick_asset(&release, arch).ok_or_else(|| {
        anyhow!(
            "Release {} has no windows-{}-portable.zip (assets: {})",
            release.tag_name,
            arch,
            release
                .assets
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    Ok(CheckResult {
        update_available: is_newer(&release.tag_name, installed.as_deref()),
        installed,
        latest: release.tag_name.clone(),
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        asset_size: asset.size,
        asset_digest: asset.digest.clone(),
        release_notes: release.body.clone(),
        release_url: release.html_url.clone(),
        program_dir,
    })
}

/// Progress callback: (downloaded_bytes, total_bytes, stage_label).
pub type Progress<'a> = dyn FnMut(u64, u64, &str) + 'a;

/// Download, verify, back up, and install an update. Returns the backup path
/// created (if any) so the caller can show it.
pub fn perform_update(
    app: &AppEntry,
    check: &CheckResult,
    keep_backups: usize,
    progress: &mut Progress,
) -> Result<Option<PathBuf>> {
    let install_dir = PathBuf::from(&app.install_dir);
    let program_dir = &check.program_dir;

    // temp working dir
    let work = std::env::temp_dir().join(format!(
        "craft-updater-{}-{}",
        app.name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let zip_path = work.join(&check.asset_name);

    // download with progress
    progress(0, check.asset_size, "Downloading");
    let client = http_client()?;
    let mut resp = client.get(&check.asset_url).send().context("downloading")?;
    if !resp.status().is_success() {
        bail!("download failed: HTTP {}", resp.status().as_u16());
    }
    let total = resp.content_length().unwrap_or(check.asset_size);
    {
        let mut out = std::fs::File::create(&zip_path)?;
        let mut buf = [0u8; 65536];
        let mut done: u64 = 0;
        loop {
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut out, &buf[..n])?;
            done += n as u64;
            progress(done, total, "Downloading");
        }
    }

    // verify checksum when available
    progress(total, total, "Verifying");
    let actual = sha256_file(&zip_path)?;
    if let Some(expected) = expected_sha(check) {
        if !actual.eq_ignore_ascii_case(&expected) {
            bail!("SHA-256 mismatch: expected {expected}, got {actual}. Nothing changed.");
        }
    }

    // extract
    progress(total, total, "Extracting");
    let stage = work.join("stage");
    let src = extract_zip(&zip_path, &stage)?;
    let has_exe = {
        let mut found = false;
        walk(&src, 0, 4, &mut |p| {
            if p.extension().and_then(|e| e.to_str()) == Some("exe") {
                found = true;
            }
        });
        found
    };
    if !has_exe {
        bail!("No .exe inside the downloaded package; refusing to install.");
    }

    // back up current program folder
    let mut backup_path = None;
    if program_dir.exists()
        && std::fs::read_dir(program_dir)
            .map(|mut r| r.next().is_some())
            .unwrap_or(false)
    {
        progress(total, total, "Backing up");
        let tag = check.installed.clone().unwrap_or_else(|| "unknown".into());
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dest = backup_root(&install_dir, &app.name).join(format!("{tag}_{stamp}"));
        copy_dir(program_dir, &dest)?;
        backup_path = Some(dest);
        prune_backups(&install_dir, &app.name, keep_backups);
    }

    // overlay install, rolling back on failure
    progress(total, total, "Installing");
    if let Err(e) = copy_dir(&src, program_dir) {
        if let Some(b) = &backup_path {
            let _ = copy_dir(b, program_dir);
        }
        return Err(e.context("install failed; backup restored"));
    }
    std::fs::write(program_dir.join(VERSION_MARKER), &check.latest)?;

    let _ = std::fs::remove_dir_all(&work);
    Ok(backup_path)
}

fn expected_sha(check: &CheckResult) -> Option<String> {
    let d = check.asset_digest.as_ref()?;
    d.strip_prefix("sha256:").map(|s| s.to_lowercase())
}

/// Restore a backup over the program folder.
pub fn restore_backup(app: &AppEntry, backup: &Path) -> Result<()> {
    let install_dir = PathBuf::from(&app.install_dir);
    let program_dir = find_program_dir(&install_dir, &app.name);
    copy_dir(backup, &program_dir).context("restoring backup")?;
    // drop the version marker; the restored folder's own marker (if any) stands
    Ok(())
}

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(parse_version("v0.13.1"), Some((0, 13, 1, 0)));
        assert_eq!(parse_version("0.8.2"), Some((0, 8, 2, 0)));
        assert_eq!(parse_version("v1.2.3.4"), Some((1, 2, 3, 4)));
        assert!(is_newer("v0.13.1", Some("0.8.2")));
        assert!(is_newer("v0.10.0", Some("v0.9.9")));
        assert!(!is_newer("v0.9.0", Some("0.9.0")));
        assert!(is_newer("v0.9.0", None)); // unknown installed → offer update
    }

    #[test]
    fn picks_right_asset() {
        let mk = |n: &str| Asset {
            name: n.into(),
            browser_download_url: format!("http://x/{n}"),
            size: 1,
            digest: None,
        };
        let r = Release {
            tag_name: "v1".into(),
            body: String::new(),
            html_url: String::new(),
            assets: vec![
                mk("app-1-windows-arm64-portable.zip"),
                mk("app-1-windows-x64-portable.zip"),
                mk("app-1-linux-x64.tar.gz"),
            ],
        };
        assert_eq!(
            pick_asset(&r, "x64").unwrap().name,
            "app-1-windows-x64-portable.zip"
        );
        assert_eq!(
            pick_asset(&r, "arm64").unwrap().name,
            "app-1-windows-arm64-portable.zip"
        );
    }

    #[test]
    fn finds_program_dir_in_portableapps_layout() {
        let tmp = std::env::temp_dir().join(format!("cu-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let app = tmp.join("App").join("PhotoCraft");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("PhotoCraft.exe"), b"x").unwrap();
        std::fs::write(tmp.join("PhotoCraftPortable.exe"), b"x").unwrap(); // launcher, ignored
        let found = find_program_dir(&tmp, "PhotoCraft");
        assert_eq!(found, app);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn copy_and_extract_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("cu-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let srcdir = tmp.join("src");
        std::fs::create_dir_all(srcdir.join("sub")).unwrap();
        std::fs::write(srcdir.join("a.txt"), b"hello").unwrap();
        std::fs::write(srcdir.join("sub").join("b.txt"), b"world").unwrap();
        let dst = tmp.join("dst");
        copy_dir(&srcdir, &dst).unwrap();
        assert_eq!(std::fs::read(dst.join("sub").join("b.txt")).unwrap(), b"world");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
