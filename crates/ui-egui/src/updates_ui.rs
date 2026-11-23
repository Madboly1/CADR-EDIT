//! In-app GitHub Release auto-updater UI (`Madboly1/CADR-EDIT`).
//!
//! - Runs at most one non-blocking background check on startup when enabled by the desktop app.
//! - Surfaces a subtle top-bar badge (`available_version`) when a newer release exists.
//! - Answers Help ▸ Check for Updates… (`app.checkForUpdates`) with a modal dialog showing the
//!   current version, latest version, scrollable release notes, live download progress bar, and a
//!   "Download & Restart to Update" action.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Mutex, PoisonError};

use egui::Align2;
#[cfg(not(target_arch = "wasm32"))]
use egui::{Color32, Stroke, StrokeKind, vec2};
use filmcraft_engine::updater::ReleaseInfo;
#[cfg(not(target_arch = "wasm32"))]
use filmcraft_engine::updater::{CURRENT_VERSION, UpdateCheckResult};
use serde_json::Value;

use crate::FilmcraftApp;
#[cfg(not(target_arch = "wasm32"))]
use crate::theme::Tokens;

/// Live progress reported by the background download thread.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, Default)]
struct DownloadProgress {
    downloaded: u64,
    total: u64,
}

/// High-level state machine of the in-app updater.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    UpToDate {
        latest: String,
    },
    Available(ReleaseInfo),
    Downloading {
        release: ReleaseInfo,
    },
    /// Download succeeded and the binary is staged on disk, waiting to apply.
    ReadyToInstall {
        release: ReleaseInfo,
        staged_path: PathBuf,
    },
    Failed(String),
}

#[cfg(not(target_arch = "wasm32"))]
enum WorkerEvent {
    Checked(Result<UpdateCheckResult, String>),
    Downloaded { release: Box<ReleaseInfo>, result: Result<PathBuf, String> },
}

pub(crate) struct Runtime {
    /// Desktop app turns the background startup check on; tests and web leave it off.
    #[cfg(not(target_arch = "wasm32"))]
    startup_check: bool,
    #[cfg(not(target_arch = "wasm32"))]
    startup_started: bool,
    #[cfg(not(target_arch = "wasm32"))]
    check_requested: bool,
    pub(crate) dialog_open: bool,
    pub(crate) status: UpdateStatus,
    #[cfg(not(target_arch = "wasm32"))]
    progress: Arc<Mutex<DownloadProgress>>,
    cancel_download: Arc<AtomicBool>,
    #[cfg(not(target_arch = "wasm32"))]
    events: Arc<Mutex<Vec<WorkerEvent>>>,
    repaint: Option<egui::Context>,
    /// Set once the detached Windows self-replace script has been launched.
    pending_close: bool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            startup_check: false,
            #[cfg(not(target_arch = "wasm32"))]
            startup_started: false,
            #[cfg(not(target_arch = "wasm32"))]
            check_requested: false,
            dialog_open: false,
            status: UpdateStatus::Idle,
            #[cfg(not(target_arch = "wasm32"))]
            progress: Arc::new(Mutex::new(DownloadProgress::default())),
            cancel_download: Arc::new(AtomicBool::new(false)),
            #[cfg(not(target_arch = "wasm32"))]
            events: Arc::new(Mutex::new(Vec::new())),
            repaint: None,
            pending_close: false,
        }
    }
}

impl FilmcraftApp {
    /// Whether the app checks GitHub Releases once asynchronously on startup.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_startup_update_check(&mut self, on: bool) {
        self.updates_rt.startup_check = on;
    }
}

/// Version string of an available update (e.g. `"0.21.0"`) for the top-bar notification badge.
pub(crate) fn available_version(app: &FilmcraftApp) -> Option<&str> {
    match &app.updates_rt.status {
        UpdateStatus::Available(r) | UpdateStatus::Downloading { release: r } | UpdateStatus::ReadyToInstall { release: r, .. } => Some(&r.version),
        _ => None,
    }
}

/// Open the updater modal without restarting an already completed or in-progress check.
pub(crate) fn open_dialog(app: &mut FilmcraftApp) {
    app.updates_rt.dialog_open = true;
    #[cfg(not(target_arch = "wasm32"))]
    if matches!(app.updates_rt.status, UpdateStatus::Idle) {
        app.updates_rt.check_requested = true;
    }
    if let Some(ctx) = &app.updates_rt.repaint {
        ctx.request_repaint();
    }
}

/// Trigger an on-demand check for updates and open the updater modal (Help ▸ Check for Updates…).
pub(crate) fn check(app: &mut FilmcraftApp) -> Result<Value, String> {
    app.updates_rt.dialog_open = true;
    #[cfg(not(target_arch = "wasm32"))]
    {
        if !matches!(app.updates_rt.status, UpdateStatus::Checking | UpdateStatus::Downloading { .. }) {
            app.updates_rt.check_requested = true;
        }
        if let Some(ctx) = &app.updates_rt.repaint {
            ctx.request_repaint();
        }
    }
    Ok(Value::Null)
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_check(rt: &mut Runtime, ctx: &egui::Context) {
    rt.status = UpdateStatus::Checking;
    let events = rt.events.clone();
    let ctx_wake = ctx.clone();
    let _ = std::thread::Builder::new().name("cadr-update-check".into()).spawn(move || {
        let outcome = std::panic::catch_unwind(|| filmcraft_engine::updater::check_latest_release(CURRENT_VERSION))
            .unwrap_or_else(|_| Err("update check failed unexpectedly".into()));
        events.lock().unwrap_or_else(PoisonError::into_inner).push(WorkerEvent::Checked(outcome));
        ctx_wake.request_repaint();
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_download(rt: &mut Runtime, release: ReleaseInfo, ctx: &egui::Context) {
    rt.cancel_download.store(false, Ordering::Relaxed);
    *rt.progress.lock().unwrap_or_else(PoisonError::into_inner) = DownloadProgress { downloaded: 0, total: release.asset_size };
    rt.status = UpdateStatus::Downloading { release: release.clone() };
    let progress = rt.progress.clone();
    let cancel = rt.cancel_download.clone();
    let events = rt.events.clone();
    let ctx_wake = ctx.clone();
    let _ = std::thread::Builder::new().name("cadr-update-download".into()).spawn(move || {
        let rel_copy = release.clone();
        let ctx_cb = ctx_wake.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut cb = |downloaded: u64, total: u64| -> bool {
                *progress.lock().unwrap_or_else(PoisonError::into_inner) = DownloadProgress { downloaded, total };
                ctx_cb.request_repaint();
                !cancel.load(Ordering::Relaxed)
            };
            filmcraft_engine::updater::download_release_asset(&rel_copy, &mut cb)
        }))
        .unwrap_or_else(|_| Err("download thread failed unexpectedly".into()));
        events.lock().unwrap_or_else(PoisonError::into_inner).push(WorkerEvent::Downloaded { release: Box::new(release), result });
        ctx_wake.request_repaint();
    });
}

/// Poll background check/download threads and start queued checks.
pub(crate) fn tick(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let rt = &mut app.updates_rt;
    rt.repaint = Some(ctx.clone());

    #[cfg(not(target_arch = "wasm32"))]
    {
        if std::mem::take(&mut rt.check_requested) {
            spawn_check(rt, ctx);
        } else if rt.startup_check && !rt.startup_started {
            rt.startup_started = true;
            // The previous start may have tried to apply an update and failed silently: say so
            // in the existing updater dialog instead of just offering the update again.
            if let Some(why) = filmcraft_engine::updater::take_last_update_failure() {
                rt.status = UpdateStatus::Failed(format!("the last update was not applied ({why})"));
                rt.dialog_open = true;
            } else {
                spawn_check(rt, ctx);
            }
        }

        let drained: Vec<WorkerEvent> = std::mem::take(&mut *rt.events.lock().unwrap_or_else(PoisonError::into_inner));
        for ev in drained {
            match ev {
                WorkerEvent::Checked(Ok(UpdateCheckResult::UpToDate { latest, .. })) => {
                    rt.status = UpdateStatus::UpToDate { latest };
                }
                WorkerEvent::Checked(Ok(UpdateCheckResult::UpdateAvailable(info))) => {
                    rt.status = UpdateStatus::Available(*info);
                }
                WorkerEvent::Checked(Err(e)) => {
                    // Stay quiet if a background startup check fails while the dialog is closed.
                    rt.status = if rt.dialog_open { UpdateStatus::Failed(e) } else { UpdateStatus::Idle };
                }
                WorkerEvent::Downloaded { release, result } => match result {
                    Ok(staged_path) => {
                        rt.status = UpdateStatus::ReadyToInstall { release: *release, staged_path };
                    }
                    Err(e) => {
                        if rt.cancel_download.load(Ordering::Relaxed) {
                            rt.status = UpdateStatus::Available(*release);
                        } else {
                            rt.status = UpdateStatus::Failed(e);
                        }
                    }
                },
            }
        }
    }
}

/// Render the updater modal dialog when open and apply ready updates on confirmation.
pub(crate) fn show(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if app.updates_rt.pending_close {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        return;
    }

    // Automatically launch self-replace once a download completes if no unsaved edits exist.
    #[cfg(not(target_arch = "wasm32"))]
    if !app.session.is_dirty()
        && let UpdateStatus::ReadyToInstall { staged_path, .. } = &app.updates_rt.status
    {
        let staged = staged_path.clone();
        match filmcraft_engine::updater::launch_windows_self_replace(&staged) {
            Ok(()) => {
                app.updates_rt.pending_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            Err(e) => {
                app.updates_rt.status = UpdateStatus::Failed(e);
            }
        }
    }

    if !app.updates_rt.dialog_open {
        return;
    }

    #[cfg(target_arch = "wasm32")]
    show_wasm_modal(app, ctx);

    #[cfg(not(target_arch = "wasm32"))]
    show_native_modal(app, ctx);
}

#[cfg(target_arch = "wasm32")]
fn show_wasm_modal(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let mut open = true;
    egui::Window::new(crate::i18n::t("CADR Studio Updates"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ui.label(crate::i18n::t("Update check is not supported in the web browser."));
            ui.add_space(8.0);
            if ui.button(crate::i18n::t("OK")).clicked() {
                app.updates_rt.dialog_open = false;
            }
        });
    if !open {
        app.updates_rt.dialog_open = false;
    }
}

#[cfg(not(target_arch = "wasm32"))]
enum ModalAction {
    None,
    Close,
    CheckAgain,
    StartDownload(Box<ReleaseInfo>),
    CancelDownload,
    InstallNow(PathBuf),
    OpenUrl(String),
}

#[cfg(not(target_arch = "wasm32"))]
fn show_native_modal(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let t = app.tokens;
    let status = app.updates_rt.status.clone();
    let progress = *app.updates_rt.progress.lock().unwrap_or_else(PoisonError::into_inner);
    let dirty = app.session.is_dirty();
    let mut open = true;
    let mut action = ModalAction::None;

    egui::Window::new(crate::i18n::t("CADR Studio Updates"))
        .id(egui::Id::unique("cadr-updates-dialog"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .fixed_size(vec2(440.0, 0.0))
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ui.set_min_width(420.0);
            action = draw_modal_body(app, ui, &t, &status, progress, dirty);
        });

    if !open {
        action = ModalAction::Close;
    }
    apply_modal_action(app, ctx, action);
}

#[cfg(not(target_arch = "wasm32"))]
fn draw_modal_body(app: &mut FilmcraftApp, ui: &mut egui::Ui, t: &Tokens, status: &UpdateStatus, progress: DownloadProgress, dirty: bool) -> ModalAction {
    let mut action = ModalAction::None;
    let latest_text = match status {
        UpdateStatus::Idle | UpdateStatus::Checking => "Checking…".to_string(),
        UpdateStatus::UpToDate { latest } => format!("v{latest}"),
        UpdateStatus::Available(r) | UpdateStatus::Downloading { release: r } | UpdateStatus::ReadyToInstall { release: r, .. } => r.tag_name.clone(),
        UpdateStatus::Failed(_) => "Unavailable".to_string(),
    };

    egui::Grid::new("cadr-update-versions").num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
        ui.label(egui::RichText::new("Current Version:").color(t.text_dim));
        ui.label(egui::RichText::new(format!("v{CURRENT_VERSION}")).strong().color(t.text));
        ui.end_row();
        ui.label(egui::RichText::new("Latest Version:").color(t.text_dim));
        ui.label(egui::RichText::new(latest_text).strong().color(t.text));
        ui.end_row();
    });
    ui.add_space(10.0);

    match status {
        UpdateStatus::Idle | UpdateStatus::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking Madboly1/CADR-EDIT on GitHub Releases…");
            });
            ui.add_space(10.0);
            if ui.button(tl!("Close")).clicked() {
                action = ModalAction::Close;
            }
        }
        UpdateStatus::UpToDate { .. } => {
            ui.label(egui::RichText::new("CADR Studio is up to date.").color(t.text));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Check Again").clicked() {
                    action = ModalAction::CheckAgain;
                }
                if ui.button(tl!("OK")).clicked() {
                    action = ModalAction::Close;
                }
            });
        }
        UpdateStatus::Available(release) => {
            draw_release_notes(ui, t, release);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if release.download_url.is_some() {
                    let btn = accent_button(ui, t, "Download & Restart to Update");
                    app.auto.add("updates.downloadAndRestart", btn.rect, "Download & Restart to Update");
                    if btn.clicked() {
                        action = ModalAction::StartDownload(Box::new(release.clone()));
                    }
                }
                if ui.button("View on GitHub").clicked() {
                    action = ModalAction::OpenUrl(release.html_url.clone());
                }
                if ui.button(tl!("Close")).clicked() {
                    action = ModalAction::Close;
                }
            });
        }
        UpdateStatus::Downloading { release } => {
            draw_release_notes(ui, t, release);
            ui.add_space(8.0);
            let frac = if progress.total > 0 { (progress.downloaded as f32 / progress.total as f32).clamp(0.0, 1.0) } else { 0.0 };
            let label = format!("Downloading {} ({:.1} / {:.1} MB)…", release.tag_name, progress.downloaded as f64 / 1e6, progress.total as f64 / 1e6);
            ui.add(egui::ProgressBar::new(frac).show_percentage().text(label));
            ui.add_space(10.0);
            if ui.button(tl!("Cancel")).clicked() {
                action = ModalAction::CancelDownload;
            }
        }
        UpdateStatus::ReadyToInstall { release, staged_path } => {
            draw_release_notes(ui, t, release);
            ui.add_space(8.0);
            if dirty {
                ui.label(
                    egui::RichText::new("You have unsaved project changes. Save your work before restarting, or restart now to install the update.")
                        .color(Color32::from_rgb(0xf0, 0xb8, 0x40)),
                );
            } else {
                ui.label("Update downloaded and verified. Ready to restart and install.");
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let btn = accent_button(ui, t, "Restart to Update");
                app.auto.add("updates.restartNow", btn.rect, "Restart to Update");
                if btn.clicked() {
                    action = ModalAction::InstallNow(staged_path.clone());
                }
                if ui.button(tl!("Cancel")).clicked() {
                    action = ModalAction::Close;
                }
            });
        }
        UpdateStatus::Failed(err) => {
            ui.label(egui::RichText::new(format!("Update failed: {err}")).color(Color32::from_rgb(0xe8, 0x64, 0x64)));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Check Again").clicked() {
                    action = ModalAction::CheckAgain;
                }
                if ui.button(tl!("Close")).clicked() {
                    action = ModalAction::Close;
                }
            });
        }
    }
    action
}

#[cfg(not(target_arch = "wasm32"))]
fn draw_release_notes(ui: &mut egui::Ui, t: &Tokens, release: &ReleaseInfo) {
    ui.label(egui::RichText::new("Release Notes").strong().color(t.text));
    ui.add_space(4.0);
    let notes = if release.release_notes.is_empty() { "No release notes provided." } else { release.release_notes.as_str() };
    egui::Frame::group(ui.style()).fill(t.panel_bg).stroke(Stroke::new(1.0, t.separator)).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("cadr-update-notes").max_height(170.0).show(ui, |ui| {
            ui.set_min_width(390.0);
            ui.label(egui::RichText::new(notes).size(12.0).color(t.text_dim));
        });
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn accent_button(ui: &mut egui::Ui, t: &Tokens, label: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(label.to_string(), Tokens::semibold(12.5), Color32::WHITE);
    let size = vec2(galley.size().x + 24.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let fill = if resp.hovered() { t.accent.gamma_multiply(0.88) } else { t.accent };
    ui.painter().rect_filled(rect, 4.0, fill);
    ui.painter().rect_stroke(rect, 4.0, Stroke::new(1.0, t.accent), StrokeKind::Inside);
    ui.painter().galley(egui::pos2(rect.center().x - galley.size().x * 0.5, rect.center().y - galley.size().y * 0.5), galley, Color32::WHITE);
    resp
}

#[cfg(not(target_arch = "wasm32"))]
fn apply_modal_action(app: &mut FilmcraftApp, ctx: &egui::Context, action: ModalAction) {
    match action {
        ModalAction::None => {}
        ModalAction::Close => {
            app.updates_rt.dialog_open = false;
        }
        ModalAction::CheckAgain => {
            app.updates_rt.check_requested = true;
            ctx.request_repaint();
        }
        ModalAction::StartDownload(release) => {
            spawn_download(&mut app.updates_rt, *release, ctx);
        }
        ModalAction::CancelDownload => {
            app.updates_rt.cancel_download.store(true, Ordering::Relaxed);
        }
        ModalAction::InstallNow(staged) => match filmcraft_engine::updater::launch_windows_self_replace(&staged) {
            Ok(()) => {
                app.updates_rt.pending_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => {
                app.updates_rt.status = UpdateStatus::Failed(e);
            }
        },
        ModalAction::OpenUrl(url) => {
            crate::links::open(app, ctx, &url);
        }
    }
}

/// Called from `on_exit`; cancels any active download cleanly.
pub(crate) fn exiting(app: &mut FilmcraftApp) {
    app.updates_rt.cancel_download.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_runtime_is_idle_and_exposes_no_badge() {
        let app = FilmcraftApp::new(filmcraft_engine::Session::default());
        assert!(!app.updates_rt.dialog_open);
        assert_eq!(app.updates_rt.status, UpdateStatus::Idle);
        assert_eq!(available_version(&app), None);
    }

    #[test]
    fn available_release_shows_badge_and_renders_modal() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        let ctx = egui::Context::default();
        app.updates_rt.status = UpdateStatus::Available(ReleaseInfo {
            tag_name: "v99.0.0".into(),
            version: "99.0.0".into(),
            name: "CADR Studio 99.0.0".into(),
            release_notes: "- Native .mogrt support\n- Auto-updater".into(),
            html_url: "https://github.com/Madboly1/CADR-EDIT/releases/tag/v99.0.0".into(),
            asset_name: Some("CADR-Studio-windows-x86_64.exe".into()),
            download_url: Some("https://github.com/Madboly1/CADR-EDIT/releases/download/v99.0.0/CADR-Studio-windows-x86_64.exe".into()),
            asset_size: 45_000_000,
            is_zip: false,
            ..Default::default()
        });
        assert_eq!(available_version(&app), Some("99.0.0"));
        open_dialog(&mut app);
        assert!(app.updates_rt.dialog_open);

        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            show(&mut app, ui.ctx());
        });
        output.textures_delta.clear();
        assert!(app.updates_rt.dialog_open);
    }
}
