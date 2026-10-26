// Craft Updater — portable updater for PhotoCraft & VectorCraft.
// No console window on Windows:
#![cfg_attr(windows, windows_subsystem = "windows")]

mod core;

use eframe::egui;
use egui::{Color32, RichText, Rounding, Stroke, Vec2};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core::{AppEntry, CheckInterval, CheckResult, Config};

// ------------------------------------------------------------------ messages

enum Msg {
    Checked(usize, Result<CheckResult, String>),
    Progress(usize, u64, u64, String),
    Updated(usize, Result<Option<PathBuf>, String>),
    Restored(usize, Result<(), String>),
    /// Background checker found an update; raise the window + toast.
    AutoUpdateFound(usize, String),
}

#[derive(Clone)]
enum Status {
    Idle,
    Checking,
    UpToDate,
    UpdateReady,
    Updating { done: u64, total: u64, stage: String },
    Error(String),
    Done(String),
}

struct AppRow {
    status: Status,
    check: Option<CheckResult>,
    backups: Vec<core::Backup>,
    show_notes: bool,
    /// Cached list of launchable .exe files; `None` until computed.
    exes: Option<Vec<PathBuf>>,
}

impl Default for AppRow {
    fn default() -> Self {
        AppRow {
            status: Status::Idle,
            check: None,
            backups: vec![],
            show_notes: false,
            exes: None,
        }
    }
}

/// Pretty launch-button label, e.g. "Launch PhotoCraft" / "Launch PhotoCraft CLI".
fn launch_label(app_name: &str, exe_stem: &str) -> String {
    let lname = app_name.to_lowercase();
    let stem = exe_stem.to_lowercase();
    if stem == lname {
        return format!("Launch {app_name}");
    }
    let rem = stem
        .strip_prefix(&lname)
        .unwrap_or(&stem)
        .trim_matches(|c| c == '-' || c == '_' || c == ' ');
    let suffix: Vec<String> = rem
        .split(|c| c == '-' || c == '_' || c == ' ')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.eq_ignore_ascii_case("cli") || s.eq_ignore_ascii_case("gui") {
                s.to_uppercase()
            } else {
                let mut ch = s.chars();
                match ch.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect();
    if suffix.is_empty() {
        format!("Launch {app_name}")
    } else {
        format!("Launch {app_name} {}", suffix.join(" "))
    }
}

fn launch_exe(exe: &Path) {
    let dir = exe.parent().map(|p| p.to_path_buf());
    let mut cmd = std::process::Command::new(exe);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let _ = cmd.spawn();
}

// ------------------------------------------------------------------ the app

struct CraftUpdater {
    cfg: Arc<Mutex<Config>>,
    cfg_path: PathBuf,
    rows: Vec<AppRow>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    arch: &'static str,
    settings_open: bool,
    first_frame: bool,
    hidden: bool,
    add_sel: String,
    pending_remove: Option<usize>,
    last_size: (f32, f32),
    #[cfg(windows)]
    _tray: Option<tray_icon::TrayIcon>,
    #[cfg(windows)]
    menu_rx: Option<Receiver<String>>,
    #[cfg(windows)]
    hwnd: Option<isize>,
    banner: Option<String>,
}

impl CraftUpdater {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme(&cc.egui_ctx);
        let cfg_path = config_path();
        let cfg = Config::load(&cfg_path);
        let rows = (0..cfg.apps.len()).map(|_| AppRow::default()).collect();
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(cfg));

        let mut app = CraftUpdater {
            cfg: shared.clone(),
            cfg_path,
            rows,
            tx: tx.clone(),
            rx,
            arch: core::current_arch(),
            settings_open: false,
            first_frame: true,
            hidden: false,
            add_sel: String::new(),
            pending_remove: None,
            last_size: (0.0, 0.0),
            #[cfg(windows)]
            _tray: None,
            #[cfg(windows)]
            menu_rx: None,
            #[cfg(windows)]
            hwnd: None,
            banner: None,
        };
        app.refresh_backups();

        #[cfg(windows)]
        {
            // Tray/menu events are delivered via handlers that also wake the
            // egui event loop, so the tray keeps working while the window is hidden.
            let (menu_tx, menu_rx) = channel::<String>();
            app._tray = build_tray(&cc.egui_ctx, menu_tx);
            app.menu_rx = Some(menu_rx);
        }

        // background checker thread (independent of the window being visible)
        spawn_checker(shared, tx, cc.egui_ctx.clone());

        // "pump" thread: keep update() ticking even while the window is hidden,
        // so tray menu actions (Open/Quit) are always processed. eframe stops
        // calling update() for a hidden window otherwise.
        {
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(200));
                ctx.request_repaint();
            });
        }
        app
    }

    fn save_cfg(&self) {
        if let Ok(c) = self.cfg.lock() {
            let _ = c.save(&self.cfg_path);
        }
    }

    fn refresh_backups(&mut self) {
        let cfg = self.cfg.lock().unwrap().clone();
        for (i, app) in cfg.apps.iter().enumerate() {
            if !app.install_dir.is_empty() {
                let dir = PathBuf::from(&app.install_dir);
                self.rows[i].backups = core::list_backups(&dir, &app.name);
            }
        }
    }

    fn check(&mut self, idx: usize) {
        let cfg = self.cfg.lock().unwrap().clone();
        let Some(app) = cfg.apps.get(idx).cloned() else {
            return;
        };
        if app.install_dir.is_empty() {
            self.rows[idx].status = Status::Error("Set the install folder first".into());
            return;
        }
        self.rows[idx].status = Status::Checking;
        let tx = self.tx.clone();
        let arch = self.arch;
        std::thread::spawn(move || {
            let r = core::check_app(&app, arch).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Checked(idx, r));
        });
    }

    fn check_all(&mut self) {
        for i in 0..self.rows.len() {
            self.check(i);
        }
    }

    fn update(&mut self, idx: usize) {
        let cfg = self.cfg.lock().unwrap().clone();
        let Some(app) = cfg.apps.get(idx).cloned() else {
            return;
        };
        let Some(check) = self.rows[idx].check.clone() else {
            return;
        };
        let keep = cfg.keep_backups;
        let tx = self.tx.clone();
        self.rows[idx].status = Status::Updating {
            done: 0,
            total: check.asset_size,
            stage: "Starting".into(),
        };
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let mut progress =
                move |d: u64, t: u64, s: &str| { let _ = tx2.send(Msg::Progress(idx, d, t, s.to_string())); };
            let r = core::perform_update(&app, &check, keep, &mut progress).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Updated(idx, r));
        });
    }

    fn update_all(&mut self) {
        let ready: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.status, Status::UpdateReady))
            .map(|(i, _)| i)
            .collect();
        for i in ready {
            self.update(i);
        }
    }

    fn restore(&mut self, idx: usize, backup: PathBuf) {
        let cfg = self.cfg.lock().unwrap().clone();
        let Some(app) = cfg.apps.get(idx).cloned() else {
            return;
        };
        let tx = self.tx.clone();
        self.rows[idx].status = Status::Updating {
            done: 0,
            total: 0,
            stage: "Restoring".into(),
        };
        std::thread::spawn(move || {
            let r = core::restore_backup(&app, &backup).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Restored(idx, r));
        });
    }

    fn add_app(&mut self, name: String, repo: String) -> usize {
        {
            let mut c = self.cfg.lock().unwrap();
            c.apps.push(AppEntry {
                name,
                repo,
                install_dir: String::new(),
            });
        }
        self.rows.push(AppRow::default());
        self.save_cfg();
        self.rows.len() - 1
    }

    fn remove_app(&mut self, idx: usize, delete_folder: bool) {
        let app = self.cfg.lock().unwrap().apps.get(idx).cloned();
        if let Some(app) = app {
            if delete_folder && !app.install_dir.is_empty() {
                let _ = std::fs::remove_dir_all(&app.install_dir);
            }
        }
        {
            let mut c = self.cfg.lock().unwrap();
            if idx < c.apps.len() {
                c.apps.remove(idx);
            }
        }
        if idx < self.rows.len() {
            self.rows.remove(idx);
        }
        self.save_cfg();
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Checked(i, Ok(c)) => {
                    if i < self.rows.len() {
                        self.rows[i].status = if c.update_available {
                            Status::UpdateReady
                        } else {
                            Status::UpToDate
                        };
                        self.rows[i].check = Some(c);
                    }
                }
                Msg::Checked(i, Err(e)) => self.set_err(i, e),
                Msg::Progress(i, d, t, s) => {
                    if i < self.rows.len() {
                        self.rows[i].status = Status::Updating { done: d, total: t, stage: s };
                    }
                }
                Msg::Updated(i, Ok(bk)) => {
                    if i < self.rows.len() {
                        let v = self
                            .rows[i]
                            .check
                            .as_ref()
                            .map(|c| c.latest.clone())
                            .unwrap_or_default();
                        self.rows[i].status = Status::Done(format!("Updated to {v}"));
                        if let Some(c) = self.rows[i].check.as_mut() {
                            c.installed = Some(v);
                            c.update_available = false;
                        }
                        let _ = bk;
                        self.rows[i].exes = None; // new build may add/rename exes
                    }
                    self.refresh_backups();
                }
                Msg::Updated(i, Err(e)) => self.set_err(i, e),
                Msg::Restored(i, Ok(())) => {
                    if i < self.rows.len() {
                        self.rows[i].status = Status::Done("Restored from backup".into());
                    }
                }
                Msg::Restored(i, Err(e)) => self.set_err(i, e),
                Msg::AutoUpdateFound(i, ver) => {
                    self.show_window(ctx);
                    self.banner = Some(format!(
                        "Update available for {}: {}",
                        self.cfg.lock().unwrap().apps.get(i).map(|a| a.name.clone()).unwrap_or_default(),
                        ver
                    ));
                    #[cfg(windows)]
                    {
                        let name = self.cfg.lock().unwrap().apps.get(i).map(|a| a.name.clone()).unwrap_or_default();
                        notify(&format!("{name} update available"), &format!("Version {ver} is ready to install."));
                    }
                }
            }
        }
    }

    fn set_err(&mut self, i: usize, e: String) {
        if i < self.rows.len() {
            self.rows[i].status = Status::Error(e);
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        #[cfg(windows)]
        {
            if let Some(h) = self.get_hwnd() {
                win_show(h, true);
            }
        }
        let _ = ctx;
        #[cfg(not(windows))]
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        ctx.request_repaint();
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        #[cfg(windows)]
        {
            if let Some(h) = self.get_hwnd() {
                win_show(h, false);
            }
            let _ = ctx;
            return;
        }
        #[cfg(not(windows))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
}

impl eframe::App for CraftUpdater {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // keep polling the tray + worker channels even when hidden
        ctx.request_repaint_after(Duration::from_millis(300));
        self.drain_messages(ctx);

        #[cfg(windows)]
        self.poll_tray(ctx);

        // first-frame startup actions
        if self.first_frame {
            self.first_frame = false;
            let cfg = self.cfg.lock().unwrap().clone();
            if cfg.start_minimized {
                self.hide_window(ctx);
            }
            if cfg.check_interval != CheckInterval::Manual {
                self.check_all();
            }
        }

        // intercept window close → minimize to tray, or quit, per the setting
        if ctx.input(|i| i.viewport().close_requested()) {
            let close_to_tray = self.cfg.lock().unwrap().close_to_tray;
            if close_to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide_window(ctx);
            } else {
                // let the close proceed and exit; drop the tray icon first
                #[cfg(windows)]
                self._tray.take();
            }
        }

        // remember the window size
        if !self.hidden {
            let sz = ctx.input(|i| i.screen_rect().size());
            if (sz.x - self.last_size.0).abs() > 3.0 || (sz.y - self.last_size.1).abs() > 3.0 {
                self.last_size = (sz.x, sz.y);
                {
                    let mut c = self.cfg.lock().unwrap();
                    c.win_w = sz.x;
                    c.win_h = sz.y;
                }
                self.save_cfg();
            }
        }

        self.ui(ctx);
    }
}

// --------------------------------------------------------------------- UI

impl CraftUpdater {
    fn ui(&mut self, ctx: &egui::Context) {
        top_bar(self, ctx);
        if self.settings_open {
            self.settings_panel(ctx);
        }
        // footer pinned to the bottom, always visible (added before the central panel)
        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::none()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0)),
            )
            .show(ctx, |ui| {
                footer(self, ui);
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(b) = self.banner.clone() {
                    banner(ui, &b, || {});
                }
                ui.add_space(6.0);
                let n = self.rows.len();
                if n == 0 {
                    self.empty_state(ui);
                } else {
                    for i in 0..n {
                        self.app_card(ui, i);
                        ui.add_space(12.0);
                    }
                    self.add_bar(ui);
                }
                ui.add_space(4.0);
            });
        });
        self.remove_modal(ctx);
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No apps yet").size(20.0).strong().color(TEXT));
            ui.add_space(6.0);
            ui.label(
                RichText::new("Add a product below to start keeping it up to date.")
                    .size(13.0)
                    .color(MUTED),
            );
        });
        ui.add_space(20.0);
        self.add_bar(ui);
    }

    /// Dropdown of catalog products not already added, plus an Add button.
    fn add_bar(&mut self, ui: &mut egui::Ui) {
        let existing: Vec<String> = self
            .cfg
            .lock()
            .unwrap()
            .apps
            .iter()
            .map(|a| a.name.clone())
            .collect();
        let available: Vec<(&str, &str)> = core::CATALOG
            .iter()
            .copied()
            .filter(|(n, _)| !existing.iter().any(|e| e == n))
            .collect();
        if available.is_empty() {
            return;
        }
        if !available.iter().any(|(n, _)| *n == self.add_sel) {
            self.add_sel = available[0].0.to_string();
        }
        egui::Frame::none()
            .fill(CARD)
            .rounding(Rounding::same(12.0))
            .inner_margin(egui::Margin::same(12.0))
            .stroke(Stroke::new(1.0_f32, CARD_BORDER))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Add app:").color(MUTED).size(13.0));
                    egui::ComboBox::from_id_salt("add_product")
                        .selected_text(self.add_sel.clone())
                        .show_ui(ui, |ui| {
                            for (name, _) in &available {
                                ui.selectable_value(
                                    &mut self.add_sel,
                                    name.to_string(),
                                    *name,
                                );
                            }
                        });
                    if accent_button(ui, "Add", ACCENT_BLUE, true).clicked() {
                        if let Some((name, repo)) =
                            available.iter().find(|(n, _)| *n == self.add_sel)
                        {
                            let name = name.to_string();
                            let repo = repo.to_string();
                            let idx = self.add_app(name.clone(), repo);
                            // ask for the folder right away
                            if let Some(dir) = pick_folder() {
                                self.cfg.lock().unwrap().apps[idx].install_dir =
                                    dir.to_string_lossy().to_string();
                                self.save_cfg();
                                self.rows[idx].backups = core::list_backups(&dir, &name);
                                self.check(idx);
                            }
                        }
                    }
                });
            });
    }

    fn remove_modal(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.pending_remove else {
            return;
        };
        let name = self
            .cfg
            .lock()
            .unwrap()
            .apps
            .get(idx)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let path = self
            .cfg
            .lock()
            .unwrap()
            .apps
            .get(idx)
            .map(|a| a.install_dir.clone())
            .unwrap_or_default();
        let mut close = false;
        egui::Window::new(RichText::new(format!("Remove {name}?")).color(TEXT))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Remove this app from Craft Updater?")
                        .color(TEXT)
                        .size(13.0),
                );
                if !path.is_empty() {
                    ui.add_space(4.0);
                    ui.label(RichText::new(&path).size(11.0).color(MUTED));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ghost_button(ui, "Remove from list", true).clicked() {
                        self.remove_app(idx, false);
                        close = true;
                    }
                    let del = egui::Button::new(
                        RichText::new("Remove + delete folder").color(Color32::WHITE).strong(),
                    )
                    .fill(RED)
                    .rounding(Rounding::same(8.0))
                    .min_size(Vec2::new(0.0, 32.0));
                    if ui.add_enabled(!path.is_empty(), del).clicked() {
                        self.remove_app(idx, true);
                        close = true;
                    }
                    if ghost_button(ui, "Cancel", true).clicked() {
                        close = true;
                    }
                });
                if !path.is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("“Delete folder” permanently removes the install folder above.")
                            .size(11.0)
                            .color(RED),
                    );
                }
            });
        if close {
            self.pending_remove = None;
        }
    }

    fn app_card(&mut self, ui: &mut egui::Ui, i: usize) {
        let app = self.cfg.lock().unwrap().apps[i].clone();
        let accent = accent_for(&app.name);
        egui::Frame::none()
            .fill(CARD)
            .rounding(Rounding::same(14.0))
            .inner_margin(egui::Margin::same(16.0))
            .stroke(Stroke::new(1.0_f32, CARD_BORDER))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    chip(ui, &app.name, accent);
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&app.name).size(18.0).strong().color(TEXT));
                        ui.label(RichText::new(&app.repo).size(12.0).color(MUTED));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.status_badge(ui, i, accent);
                    });
                });

                ui.add_space(10.0);
                // version line
                ui.horizontal(|ui| {
                    let (inst, latest) = self.rows[i]
                        .check
                        .as_ref()
                        .map(|c| {
                            (
                                c.installed.clone().unwrap_or_else(|| "—".into()),
                                c.latest.clone(),
                            )
                        })
                        .unwrap_or(("—".into(), "—".into()));
                    kv(ui, "Installed", &inst);
                    ui.add_space(24.0);
                    kv(ui, "Latest", &latest);
                });

                // progress bar while updating
                if let Status::Updating { done, total, stage } = self.rows[i].status.clone() {
                    ui.add_space(8.0);
                    let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .fill(accent)
                            .text(format!("{stage}  {}", human(done, total))),
                    );
                }

                // launch buttons, one per .exe in the program folder
                if self.rows[i].exes.is_none() && !app.install_dir.is_empty() {
                    let pdir = core::find_program_dir(&PathBuf::from(&app.install_dir), &app.name);
                    self.rows[i].exes = Some(core::list_executables(&pdir));
                }
                if let Some(exes) = self.rows[i].exes.clone() {
                    if !exes.is_empty() {
                        ui.add_space(10.0);
                        ui.horizontal_wrapped(|ui| {
                            for exe in &exes {
                                let stem =
                                    exe.file_stem().and_then(|s| s.to_str()).unwrap_or("app");
                                let label = launch_label(&app.name, stem);
                                if accent_button(ui, &label, accent, true).clicked() {
                                    launch_exe(exe);
                                }
                            }
                        });
                    }
                }

                ui.add_space(12.0);
                // actions
                ui.horizontal(|ui| {
                    let ready = matches!(self.rows[i].status, Status::UpdateReady);
                    let busy = matches!(
                        self.rows[i].status,
                        Status::Checking | Status::Updating { .. }
                    );
                    if accent_button(ui, "Update", accent, ready && !busy).clicked() {
                        self.update(i);
                    }
                    if ghost_button(ui, "Check", !busy).clicked() {
                        self.check(i);
                    }
                    if ghost_button(ui, "Open folder", !app.install_dir.is_empty()).clicked() {
                        open_folder(&app.install_dir);
                    }
                    if ghost_button(ui, "Set folder…", true).clicked() {
                        if let Some(dir) = pick_folder() {
                            self.cfg.lock().unwrap().apps[i].install_dir =
                                dir.to_string_lossy().to_string();
                            self.save_cfg();
                            self.rows[i].backups = core::list_backups(&dir, &app.name);
                            self.rows[i].exes = None;
                            self.check(i);
                        }
                    }
                    let has_notes = self.rows[i]
                        .check
                        .as_ref()
                        .map(|c| !c.release_notes.is_empty())
                        .unwrap_or(false);
                    if has_notes && ghost_button(ui, "Notes", true).clicked() {
                        self.rows[i].show_notes = !self.rows[i].show_notes;
                    }
                    let rel_url = self.rows[i]
                        .check
                        .as_ref()
                        .map(|c| c.release_url.clone())
                        .filter(|u| !u.is_empty());
                    if let Some(url) = rel_url {
                        if ghost_button(ui, "GitHub", true).clicked() {
                            open_folder(&url); // open_folder uses the OS opener, which handles URLs too
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Remove").color(RED))
                                    .fill(INPUT)
                                    .rounding(Rounding::same(8.0))
                                    .min_size(Vec2::new(0.0, 32.0)),
                            )
                            .clicked()
                        {
                            self.pending_remove = Some(i);
                        }
                    });
                });

                // install path hint
                ui.add_space(6.0);
                let path_txt = if app.install_dir.is_empty() {
                    "No folder set".to_string()
                } else {
                    app.install_dir.clone()
                };
                ui.label(RichText::new(path_txt).size(11.0).color(MUTED));

                // notes
                if self.rows[i].show_notes {
                    if let Some(c) = &self.rows[i].check {
                        ui.add_space(8.0);
                        egui::Frame::none()
                            .fill(INPUT)
                            .rounding(Rounding::same(8.0))
                            .inner_margin(egui::Margin::same(10.0))
                            .show(ui, |ui| {
                                egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                                    ui.label(RichText::new(&c.release_notes).size(12.0).color(TEXT));
                                });
                            });
                    }
                }

                // backups
                if !self.rows[i].backups.is_empty() {
                    ui.add_space(8.0);
                    ui.collapsing(
                        RichText::new(format!("Backups ({})", self.rows[i].backups.len()))
                            .color(MUTED)
                            .size(12.0),
                        |ui| {
                            let backups = self.rows[i].backups.clone();
                            for b in backups {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&b.label).size(12.0).color(TEXT));
                                    if ghost_button(ui, "Restore", true).clicked() {
                                        self.restore(i, b.path.clone());
                                    }
                                });
                            }
                        },
                    );
                }
            });
    }

    fn status_badge(&self, ui: &mut egui::Ui, i: usize, accent: Color32) {
        let (text, color) = match &self.rows[i].status {
            Status::Idle => ("Not checked", MUTED),
            Status::Checking => ("Checking…", accent),
            Status::UpToDate => ("Up to date", GREEN),
            Status::UpdateReady => ("Update available", accent),
            Status::Updating { .. } => ("Working…", accent),
            Status::Done(s) => (s.as_str(), GREEN),
            Status::Error(_) => ("Error", RED),
        };
        egui::Frame::none()
            .fill(color.linear_multiply(0.18))
            .rounding(Rounding::same(20.0))
            .inner_margin(egui::Margin::symmetric(10.0, 4.0))
            .show(ui, |ui| {
                ui.label(RichText::new(text).size(12.0).color(color).strong());
            });
        if let Status::Error(e) = &self.rows[i].status {
            ui.label(RichText::new("ⓘ").color(RED)).on_hover_text(e);
        }
    }

    fn settings_panel(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        egui::Window::new(RichText::new("Settings").color(TEXT))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                let mut cfg = self.cfg.lock().unwrap().clone();
                ui.label(RichText::new("Check for updates").color(MUTED).size(12.0));
                egui::ComboBox::from_id_salt("interval")
                    .selected_text(cfg.check_interval.label())
                    .show_ui(ui, |ui| {
                        for opt in CheckInterval::ALL {
                            ui.selectable_value(&mut cfg.check_interval, opt.clone(), opt.label());
                        }
                    });
                ui.add_space(8.0);
                ui.add(egui::Slider::new(&mut cfg.keep_backups, 0..=10).text("Backups to keep"));
                ui.add_space(8.0);
                ui.checkbox(&mut cfg.notify_on_update, "Show a pop-up when an update is found");
                ui.checkbox(&mut cfg.close_to_tray, "Close button (X) minimizes to the tray");
                ui.checkbox(&mut cfg.start_minimized, "Start minimized to the tray");
                if ui
                    .checkbox(&mut cfg.launch_at_login, "Launch when I sign in to Windows")
                    .changed()
                {
                    #[cfg(windows)]
                    set_autostart(cfg.launch_at_login);
                }
                *self.cfg.lock().unwrap() = cfg;
                self.save_cfg();
            });
        self.settings_open = open;
    }
}

// --------------------------------------------------------- small UI helpers

fn top_bar(app: &mut CraftUpdater, ctx: &egui::Context) {
    egui::TopBottomPanel::top("top")
        .frame(egui::Frame::none().fill(BG).inner_margin(egui::Margin::symmetric(16.0, 12.0)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Craft Updater").size(20.0).strong().color(TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ghost_button(ui, "⚙ Settings", true).clicked() {
                        app.settings_open = !app.settings_open;
                    }
                    let any_ready = app.rows.iter().any(|r| matches!(r.status, Status::UpdateReady));
                    if accent_button(ui, "Update all", ACCENT_BLUE, any_ready).clicked() {
                        app.update_all();
                    }
                    if ghost_button(ui, "Check all", true).clicked() {
                        app.check_all();
                    }
                });
            });
        });
}

fn footer(_app: &mut CraftUpdater, ui: &mut egui::Ui) {
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("Craft Updater v{}  ·  {}", env!("CARGO_PKG_VERSION"), core::current_arch()))
                .size(11.0)
                .color(MUTED),
        );
    });
}

fn banner(ui: &mut egui::Ui, text: &str, _on_close: impl FnOnce()) {
    egui::Frame::none()
        .fill(ACCENT_BLUE.linear_multiply(0.18))
        .rounding(Rounding::same(10.0))
        .inner_margin(egui::Margin::same(12.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("🔔").size(16.0));
                ui.label(RichText::new(text).color(TEXT).strong());
            });
        });
}

fn chip(ui: &mut egui::Ui, name: &str, accent: Color32) {
    let letters: String = name.chars().take(1).collect::<String>()
        + &name.chars().filter(|c| c.is_uppercase()).nth(1).map(|c| c.to_string()).unwrap_or_default();
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(44.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, Rounding::same(10.0), accent.linear_multiply(0.9));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        letters.to_uppercase(),
        egui::FontId::proportional(18.0),
        Color32::WHITE,
    );
}

fn kv(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.vertical(|ui| {
        ui.label(RichText::new(k).size(11.0).color(MUTED));
        ui.label(RichText::new(v).size(14.0).strong().color(TEXT));
    });
}

fn accent_button(ui: &mut egui::Ui, label: &str, accent: Color32, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).color(Color32::WHITE).strong())
            .fill(if enabled { accent } else { accent.linear_multiply(0.4) })
            .rounding(Rounding::same(8.0))
            .min_size(Vec2::new(92.0, 32.0)),
    )
}

fn ghost_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).color(TEXT))
            .fill(INPUT)
            .rounding(Rounding::same(8.0))
            .min_size(Vec2::new(0.0, 32.0)),
    )
}

fn human(done: u64, total: u64) -> String {
    let mb = |b: u64| b as f64 / 1_048_576.0;
    if total > 0 {
        format!("{:.1} / {:.1} MB", mb(done), mb(total))
    } else {
        format!("{:.1} MB", mb(done))
    }
}

fn accent_for(name: &str) -> Color32 {
    match name {
        "PhotoCraft" => ACCENT_BLUE,
        "VectorCraft" => ACCENT_AMBER,
        _ => ACCENT_BLUE,
    }
}

// ------------------------------------------------------------------ palette

const BG: Color32 = Color32::from_rgb(0x0e, 0x0f, 0x13);
const CARD: Color32 = Color32::from_rgb(0x17, 0x19, 0x20);
const CARD_BORDER: Color32 = Color32::from_rgb(0x25, 0x28, 0x31);
const INPUT: Color32 = Color32::from_rgb(0x20, 0x23, 0x2c);
const TEXT: Color32 = Color32::from_rgb(0xe9, 0xec, 0xf2);
const MUTED: Color32 = Color32::from_rgb(0x8b, 0x92, 0xa3);
const ACCENT_BLUE: Color32 = Color32::from_rgb(0x3b, 0x82, 0xf6);
const ACCENT_AMBER: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
const GREEN: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
const RED: Color32 = Color32::from_rgb(0xf8, 0x71, 0x71);

fn theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0_f32, CARD_BORDER);
    v.override_text_color = Some(TEXT);
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.inactive.bg_fill = INPUT;
    v.widgets.hovered.bg_fill = CARD_BORDER;
    v.widgets.active.bg_fill = ACCENT_BLUE;
    v.selection.bg_fill = ACCENT_BLUE.linear_multiply(0.5);
    ctx.set_visuals(v);
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    ctx.set_style(style);
}

// --------------------------------------------------------- platform helpers

fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("craft-updater.toml")))
        .unwrap_or_else(|| PathBuf::from("craft-updater.toml"))
}

fn open_folder(dir: &str) {
    if dir.is_empty() {
        return;
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// Minimal native folder picker via PowerShell on Windows; no extra crate.
fn pick_folder() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let ps = r#"Add-Type -AssemblyName System.Windows.Forms;$f=New-Object System.Windows.Forms.FolderBrowserDialog;if($f.ShowDialog() -eq 'OK'){[Console]::Out.Write($f.SelectedPath)}"#;
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-STA", "-Command", ps])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

// --------------------------------------------------------- background checker

fn spawn_checker(cfg: Arc<Mutex<Config>>, tx: Sender<Msg>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let mut last = Instant::now();
        let mut did_startup = false;
        loop {
            std::thread::sleep(Duration::from_secs(15));
            let c = cfg.lock().unwrap().clone();
            let interval = c.check_interval.as_duration();

            let due = match interval {
                Some(d) => last.elapsed() >= d,
                None => false,
            };
            // OnStartup handled by the GUI's first frame; here only timed polls
            if !due {
                let _ = did_startup;
                continue;
            }
            last = Instant::now();
            let arch = core::current_arch();
            for (i, app) in c.apps.iter().enumerate() {
                if app.install_dir.is_empty() {
                    continue;
                }
                match core::check_app(app, arch) {
                    Ok(res) => {
                        let up = res.update_available;
                        let ver = res.latest.clone();
                        let _ = tx.send(Msg::Checked(i, Ok(res)));
                        if up && c.notify_on_update {
                            let _ = tx.send(Msg::AutoUpdateFound(i, ver));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Checked(i, Err(e.to_string())));
                    }
                }
                ctx.request_repaint();
            }
            did_startup = true;
        }
    });
}

// ------------------------------------------------------------- Windows tray

#[cfg(windows)]
fn win_show(hwnd: isize, show: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW,
    };
    let h = hwnd as windows_sys::Win32::Foundation::HWND;
    unsafe {
        if show {
            ShowWindow(h, SW_SHOW);
            ShowWindow(h, SW_RESTORE); // un-minimize if needed
            SetForegroundWindow(h);
        } else {
            ShowWindow(h, SW_HIDE);
        }
    }
}

#[cfg(windows)]
impl CraftUpdater {
    /// Find our own top-level window by title and cache its handle.
    fn get_hwnd(&mut self) -> Option<isize> {
        if self.hwnd.is_none() {
            use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
            let title: Vec<u16> = "Craft Updater"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let h = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
            if !h.is_null() {
                self.hwnd = Some(h as isize);
            }
        }
        self.hwnd
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        // The window is shown/hidden and the app is quit directly inside the
        // tray handlers (independent of the UI loop). Here we only need to run
        // the check, which needs app state. The handler already showed the window.
        let mut ids: Vec<String> = Vec::new();
        if let Some(rx) = &self.menu_rx {
            while let Ok(id) = rx.try_recv() {
                ids.push(id);
            }
        }
        for id in ids {
            match id.as_str() {
                "open" => self.hidden = false, // handler already restored it; sync state
                "check" => {
                    self.hidden = false;
                    self.check_all();
                }
                _ => {}
            }
        }
        let _ = ctx;
    }
}

/// Find our window by title and show/hide it via Win32, with no dependency on
/// eframe's loop (which is asleep while the window is hidden).
#[cfg(windows)]
fn show_self_window(show: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
    let title: Vec<u16> = "Craft Updater"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if !h.is_null() {
        win_show(h as isize, show);
    }
}

#[cfg(windows)]
fn build_tray(ctx: &egui::Context, menu_tx: Sender<String>) -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    // Handlers act directly (Win32 show / process exit) so the tray works while
    // the window is hidden and the UI loop is asleep. They also forward to a
    // channel + repaint so the visible UI can react (e.g. run a check).
    // Handlers must be Send + Sync; mpsc::Sender is !Sync, so wrap it.
    let menu_tx = Arc::new(Mutex::new(menu_tx));
    let ctx_menu = ctx.clone();
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        match ev.id.0.as_str() {
            "quit" => std::process::exit(0),
            "open" => show_self_window(true),
            "check" => show_self_window(true),
            _ => {}
        }
        if let Ok(tx) = menu_tx.lock() {
            let _ = tx.send(ev.id.0.clone());
        }
        ctx_menu.request_repaint();
    }));
    let ctx_tray = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
        // Left-click opens the window (menu is on right-click).
        if let TrayIconEvent::Click { button, .. } = ev {
            if button == tray_icon::MouseButton::Left {
                show_self_window(true);
            }
        }
        ctx_tray.request_repaint();
    }));

    let icon = load_tray_icon()?;
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open Craft Updater", true, None);
    let check = MenuItem::with_id("check", "Check for updates now", true, None);
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    menu.append_items(&[&open, &check, &PredefinedMenuItem::separator(), &quit])
        .ok()?;
    TrayIconBuilder::new()
        .with_tooltip("Craft Updater")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(icon)
        .build()
        .ok()
}

#[cfg(windows)]
fn load_tray_icon() -> Option<tray_icon::Icon> {
    let bytes = include_bytes!("../assets/tray.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()
}

#[cfg(windows)]
fn notify(title: &str, body: &str) {
    use tauri_winrt_notification::Toast;
    let _ = Toast::new(Toast::POWERSHELL_APP_ID)
        .title(title)
        .text1(body)
        .show();
}

#[cfg(windows)]
fn set_autostart(enable: bool) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok((run, _)) =
        hkcu.create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
    else {
        return;
    };
    if enable {
        if let Ok(exe) = std::env::current_exe() {
            let _ = run.set_value("CraftUpdater", &format!("\"{}\" --minimized", exe.display()));
        }
    } else {
        let _ = run.delete_value("CraftUpdater");
    }
}

// --------------------------------------------------------------------- main

fn main() -> eframe::Result<()> {
    let icon = load_window_icon();
    // restore the last window size
    let saved = Config::load(&config_path());
    let w = saved.win_w.max(460.0);
    let h = saved.win_h.max(480.0);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([w, h])
        .with_min_inner_size([460.0, 480.0])
        .with_title("Craft Updater");
    if let Some(ic) = icon {
        viewport = viewport.with_icon(Arc::new(ic));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Craft Updater",
        options,
        Box::new(|cc| Ok(Box::new(CraftUpdater::new(cc)))),
    )
}

fn load_window_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width: w,
        height: h,
    })
}
