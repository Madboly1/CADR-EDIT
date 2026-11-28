//! The Motion presets card at the top of the Effects panel: pick an animation (Fade In, Slide Up,
//! Slide In, Pop, Slow Zoom, Spin In), set its length, easing and direction, and apply it to the
//! selected clips. Applying runs `motion.applyPreset`, which writes real keyframes on the clip's
//! own Motion and Opacity effects, so the result stays editable in Effect Controls.
//!
//! Automation ids: `motion.kind.<kind>`, `motion.easing.<name>`, `motion.direction.<dir>`,
//! `motion.duration`, `motion.apply`.

use egui::{Rect, Sense, vec2};
use serde_json::json;

use crate::FilmcraftApp;
use crate::theme::Tokens;

const KINDS: &[(&str, &str)] =
    &[("fadeIn", "Fade In"), ("slideUp", "Slide Up"), ("slideIn", "Slide In"), ("pop", "Pop"), ("slowZoom", "Slow Zoom"), ("spinIn", "Spin In")];

const EASINGS: &[(&str, &str)] = &[("easeOut", "Ease Out"), ("easeIn", "Ease In"), ("smooth", "Smooth"), ("linear", "Linear")];

const DIRECTIONS: &[(&str, &str)] = &[("up", "↑"), ("down", "↓"), ("left", "←"), ("right", "→")];

/// The directions a kind understands (none for kinds without a direction).
fn directions(kind: &str) -> &'static [&'static str] {
    match kind {
        "slideUp" | "slideIn" => &["up", "down", "left", "right"],
        "spinIn" => &["left", "right"],
        "slowZoom" => &["up", "down"],
        _ => &[],
    }
}

/// Draw the card. Returns nothing; applying sets `app.ui.status` with the outcome.
pub fn card(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    let t = app.tokens;
    let mut apply = false;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(tl!("Motion Presets")).font(Tokens::ui(12.0)).color(t.text));
    });
    ui.horizontal_wrapped(|ui| {
        ui.add_space(8.0);
        for (id, label) in KINDS {
            let on = app.ui.motion_kind == *id;
            let resp = ui.selectable_label(on, crate::i18n::t(label).to_string());
            app.auto.add(&format!("motion.kind.{id}"), resp.rect, label);
            if resp.clicked() {
                app.ui.motion_kind = (*id).to_string();
                app.ui.motion_dir = None;
            }
        }
    });
    let kind = app.ui.motion_kind.clone();
    let dirs = directions(&kind);
    let dir =
        app.ui.motion_dir.clone().filter(|d| dirs.contains(&d.as_str())).unwrap_or_else(|| filmcraft_engine::presets::motion_default_dir(&kind).to_string());
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        let enabled = kind != "slowZoom";
        let slider = egui::Slider::new(&mut app.ui.motion_secs, 0.1..=5.0).suffix(" s").step_by(0.1).text(crate::i18n::t("Length"));
        let resp = ui.add_enabled(enabled, slider);
        app.auto.add("motion.duration", resp.rect, "Motion length");
    });
    ui.horizontal_wrapped(|ui| {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(tl!("Easing")).font(Tokens::ui(11.0)).color(t.text_dim));
        for (id, label) in EASINGS {
            let resp = ui.selectable_label(app.ui.motion_easing == *id, crate::i18n::t(label).to_string());
            app.auto.add(&format!("motion.easing.{id}"), resp.rect, label);
            if resp.clicked() {
                app.ui.motion_easing = (*id).to_string();
            }
        }
    });
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(tl!("Direction")).font(Tokens::ui(11.0)).color(t.text_dim));
        for (id, glyph) in DIRECTIONS {
            if !dirs.contains(id) {
                continue;
            }
            let resp = ui.selectable_label(dir == *id, *glyph);
            app.auto.add(&format!("motion.direction.{id}"), resp.rect, id);
            if resp.clicked() {
                app.ui.motion_dir = Some((*id).to_string());
            }
        }
        if dirs.is_empty() {
            ui.label(egui::RichText::new("-").color(t.text_dim));
        }
    });
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        let (r, resp) = ui.allocate_exact_size(vec2((ui.available_width() - 8.0).max(80.0), 24.0), Sense::click());
        let fill = if resp.hovered() { t.accent } else { t.field_border };
        ui.painter().rect_filled(r, 3.0, fill);
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, tl!("Apply to Selected Clips"), Tokens::ui(12.0), t.text);
        app.auto.add("motion.apply", Rect::from_min_size(r.min, r.size()), "Apply motion preset");
        apply = resp.clicked();
    });
    ui.add_space(6.0);
    ui.separator();
    if apply {
        let r = app
            .session
            .execute("motion.applyPreset", json!({"kind": kind, "duration": f64::from(app.ui.motion_secs), "easing": app.ui.motion_easing, "direction": dir}));
        app.ui.status = match r {
            Ok(_) => crate::i18n::t("Motion preset applied").to_string(),
            Err(e) => e.to_string(),
        };
    }
}
