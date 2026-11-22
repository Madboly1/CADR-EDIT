//! Home / Import mode (header Home icon, File ▸ Home, and header "Import"): start a new project,
//! open an existing project, jump to a recent project, or import media / demo content.

use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use serde_json::json;

use crate::FilmcraftApp;
use crate::icons::Icon;
use crate::state::Mode;
use crate::theme::Tokens;

/// Recent projects listed on Home, matching the 10-item cap stored in `preferences.json`.
const RECENT_ROWS: usize = 10;
const RECENT_ROW_H: f32 = 30.0;

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    ui.painter().rect_filled(rect, t.radius, t.panel_bg);
    let recent_paths: Vec<String> = app.session.prefs.general.recent_projects.iter().take(RECENT_ROWS).cloned().collect();
    let recent_rows = recent_paths.len().max(1);
    let content_w = (rect.width() - 16.0).max(0.0);
    let total_h = 112.0 + 24.0 + recent_rows as f32 * RECENT_ROW_H + 40.0 + 66.0 + 150.0 + 32.0;

    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        egui::ScrollArea::vertical().id_salt("home-import-scroll").auto_shrink([false, false]).show(ui, |ui| {
            let (area, _) = ui.allocate_exact_size(vec2(content_w, total_h.max(rect.height())), Sense::hover());
            let ctx = ui.ctx().clone();

            // Header
            ui.painter().text(area.min + vec2(24.0, 28.0), Align2::LEFT_CENTER, tl!("Home"), Tokens::semibold(22.0), t.text);

            // Primary project actions: New Project, Open Project…, and (mid-session) return to open project
            let mut x = area.min.x + 24.0;
            let btn_y = area.min.y + 52.0;
            let btn_h = 36.0;
            if action_button(
                app,
                ui,
                Rect::from_min_size(pos2(x, btn_y), vec2(160.0, btn_h)),
                "import.file.newProject",
                Icon::NewItem,
                tl!("New Project"),
                true,
            ) {
                let _ = crate::menus::invoke(app, &ctx, "file.newProject", json!({}));
            }
            x += 172.0;
            if action_button(app, ui, Rect::from_min_size(pos2(x, btn_y), vec2(170.0, btn_h)), "import.file.open", Icon::Folder, tl!("Open Project…"), false)
            {
                let _ = crate::menus::invoke(app, &ctx, "file.open", json!({}));
            }
            x += 182.0;
            if has_active_project(app) {
                let title = if app.session.is_dirty() { tlf!("{name} - Edited", name = app.session.project.name) } else { app.session.project.name.clone() };
                let label = tlf!("Edit: {title}", title);
                let max_w = (area.max.x - x - 24.0).clamp(120.0, 320.0);
                if action_button(app, ui, Rect::from_min_size(pos2(x, btn_y), vec2(max_w, btn_h)), "import.currentProject", Icon::Sequence, &label, false) {
                    app.ui.mode = Mode::Edit;
                }
            }

            // Recent projects (name + full path)
            let import_y = recent(app, ui, area, 114.0, &recent_paths);

            // Media & demo sources
            ui.painter().text(area.min + vec2(24.0, import_y), Align2::LEFT_CENTER, tl!("Import"), Tokens::semibold(15.0), t.text);
            ui.painter().text(
                area.min + vec2(24.0, import_y + 24.0),
                Align2::LEFT_CENTER,
                tl!("Add media to your project. Drop files or folders anywhere in the window, or choose a source below."),
                Tokens::ui(12.5),
                t.text_dim,
            );
            let cards = [
                (tl!("Browse files…"), "file.import", tl!("Movies, audio and stills from disk"), Icon::Folder, true),
                (tl!("Demo footage"), "file.importDemoFootage", tl!("Six procedural 1080p clips with sound"), Icon::Film, true),
                (tl!("Demo project"), "file.openDemoProject", tl!("A cut sequence with transitions, effects and music"), Icon::Sequence, false),
                (tl!("Bars and Tone"), "file.newBarsAndTone", tl!("SMPTE HD bars with 1 kHz reference tone"), Icon::Grid, true),
            ];
            let cw = 260.0;
            let cards_y = import_y + 44.0;
            for (i, (title, cmd, sub, icon, edit_immediately)) in cards.iter().enumerate() {
                let r = Rect::from_min_size(area.min + vec2(24.0 + i as f32 * (cw + 16.0), cards_y), vec2(cw, 150.0));
                let resp = ui.interact(r, egui::Id::unique(("imp", *cmd)), Sense::click());
                app.auto.add(&format!("import.{cmd}"), r, title);
                ui.painter().rect_filled(r, 8.0, if resp.hovered() { t.hover } else { t.tl_header_bg });
                ui.painter().text(r.min + vec2(16.0, 110.0), Align2::LEFT_CENTER, *title, Tokens::semibold(14.0), t.text);
                ui.painter().text(r.min + vec2(16.0, 132.0), Align2::LEFT_CENTER, *sub, Tokens::ui(11.5), t.text_dim);
                crate::icons::paint(ui.painter(), Rect::from_min_size(r.min + vec2(16.0, 18.0), vec2(56.0, 56.0)), *icon, Color32::from_rgb(140, 150, 255));
                if resp.clicked() {
                    let _ = crate::menus::invoke(app, &ctx, cmd, json!({}));
                    // Project-replacing commands (`file.openDemoProject`) switch to Edit inside
                    // `menus::invoke` / `clip_dialogs::leave` only once the project actually loads
                    // (so Home stays visible while an unsaved-changes prompt asks).
                    if *edit_immediately {
                        app.ui.mode = Mode::Edit;
                    }
                }
            }
        });
    });
}

/// Whether the session holds an open project worth returning to (saved file, items, or unsaved edits).
fn has_active_project(app: &FilmcraftApp) -> bool {
    app.session.path.is_some() || !app.session.project.items.is_empty() || app.session.is_dirty()
}

fn action_button(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect, auto_id: &str, icon: Icon, label: &str, primary: bool) -> bool {
    let t = app.tokens;
    let resp = ui.interact(rect, egui::Id::unique(("home-btn", auto_id)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    app.auto.add(auto_id, rect, label);
    let fill = if primary {
        if resp.hovered() { t.accent.gamma_multiply(1.12) } else { t.accent }
    } else if resp.hovered() {
        t.hover
    } else {
        t.tl_header_bg
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    if !primary {
        ui.painter().rect_stroke(rect, 6.0, Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    }
    let fg = if primary { Color32::WHITE } else { t.text };
    let p = ui.painter().with_clip_rect(rect.shrink(4.0));
    crate::icons::paint(&p, Rect::from_center_size(pos2(rect.min.x + 20.0, rect.center().y), vec2(15.0, 15.0)), icon, fg);
    p.text(pos2(rect.min.x + 36.0, rect.center().y), Align2::LEFT_CENTER, label, Tokens::semibold(13.0), fg);
    resp.clicked()
}

/// Home's Recent list at `y`: the recent projects (newest first, file name with its full path
/// beside it), a click opens one (prompting to save first if the current project has unsaved work).
/// Returns where the next section starts.
fn recent(app: &mut FilmcraftApp, ui: &mut egui::Ui, area: Rect, y: f32, paths: &[String]) -> f32 {
    let t = app.tokens;
    ui.painter().text(area.min + vec2(24.0, y), Align2::LEFT_CENTER, tl!("Recent"), Tokens::semibold(15.0), t.text);
    if paths.is_empty() {
        ui.painter().text(area.min + vec2(24.0, y + 26.0), Align2::LEFT_CENTER, tl!("No recent projects"), Tokens::ui(12.5), t.text_dim);
        return y + 18.0 + RECENT_ROW_H + 28.0;
    }
    let row_w = 920.0_f32.min(area.width() - 32.0).max(0.0);
    let mut open = None;
    for (i, path) in paths.iter().enumerate() {
        let row = Rect::from_min_size(area.min + vec2(16.0, y + 18.0 + i as f32 * RECENT_ROW_H), vec2(row_w, RECENT_ROW_H));
        let name = crate::menus::recent_name(path);
        let resp =
            ui.interact(row, egui::Id::unique(("imp-recent", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(path.as_str());
        app.auto.add(&format!("import.recent.{i}"), row, &name);
        if resp.hovered() {
            ui.painter().rect_filled(row, 4.0, t.hover);
        }
        let p = ui.painter().with_clip_rect(row);
        crate::icons::paint(&p, Rect::from_center_size(pos2(row.min.x + 16.0, row.center().y), vec2(14.0, 14.0)), Icon::Sequence, t.text_dim);
        let name_r = p.text(pos2(row.min.x + 34.0, row.center().y), Align2::LEFT_CENTER, &name, Tokens::semibold(13.0), t.text);
        p.text(pos2(name_r.max.x + 14.0, row.center().y), Align2::LEFT_CENTER, path.as_str(), Tokens::ui(11.5), t.text_dim);
        if resp.clicked() {
            open = Some(path.clone());
        }
    }
    if let Some(path) = open {
        let ctx = ui.ctx().clone();
        // Unsaved changes: `menus::invoke` routes through `clip_dialogs::ask_before_open` (which
        // remembers `editAfterOpen` while on Home and keeps Home open until Save / Don't Save).
        let _ = crate::menus::invoke(app, &ctx, "file.open", json!({"path": path}));
    }
    y + 18.0 + paths.len() as f32 * RECENT_ROW_H + 28.0
}
