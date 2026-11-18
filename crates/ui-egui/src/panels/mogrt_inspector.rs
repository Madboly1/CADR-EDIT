//! Dynamic inspector UI for Motion Graphics Template (`.mogrt`) clips.
//!
//! Rendered inside the Properties panel whenever a timeline clip carrying
//! [`MogrtClipState`](filmcraft_project::MogrtClipState) is selected. Controls are generated
//! dynamically from `definition.json`'s `clientControls`, grouped under collapsible headers
//! (`type: 10`), with empty/promotional read-only info strings (`type: 8`) hidden automatically.

use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use filmcraft_project::ClipId;
use filmcraft_project::mogrt::{MogrtClipState, MogrtControl, MogrtControlKind, MogrtControlValue, MogrtMenuOption};
use serde_json::{Value, json};

use crate::FilmcraftApp;
use crate::icons::{self, Icon};
use crate::theme::Tokens;
use crate::widgets::HotNumberOptions;

const ROW_H: f32 = 28.0;
const LABEL_W: f32 = 134.0;

struct InspectorCtx<'a> {
    app: &'a mut FilmcraftApp,
    clip: ClipId,
    mogrt: &'a MogrtClipState,
    actions: &'a mut Vec<(String, Value)>,
}

/// Render the **Motion Graphics Template** section inside the Properties panel for `clip`.
pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, clip: ClipId, mogrt: &MogrtClipState, actions: &mut Vec<(String, Value)>) {
    let t = app.tokens;
    let key = format!("props:mogrt:{}", clip.0);
    let open = !app.ui.collapsed_fx.contains(&key);

    ui.add_space(4.0);
    let (hr, hresp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    ui.painter().line_segment([hr.left_top(), hr.right_top()], Stroke::new(1.0, t.separator));
    icons::paint(
        ui.painter(),
        Rect::from_center_size(pos2(hr.min.x + 6.0, hr.center().y), vec2(10.0, 10.0)),
        if open { Icon::ChevronDown } else { Icon::ChevronRight },
        t.text_dim,
    );
    ui.painter().text(pos2(hr.min.x + 18.0, hr.center().y), Align2::LEFT_CENTER, tl!("Motion Graphics Template"), Tokens::semibold(13.0), t.text);

    // Reset all overrides button in section header
    let reset_rect = Rect::from_center_size(pos2(hr.max.x - 16.0, hr.center().y), vec2(14.0, 14.0));
    icons::paint(ui.painter(), reset_rect, Icon::Reset, if mogrt.overrides.is_empty() { t.text_faint } else { t.text_dim });
    let reset_resp =
        ui.interact(reset_rect.expand(2.0), egui::Id::unique(("mogrt-reset-all", clip.0)), Sense::click()).on_hover_text(tl!("Reset All Parameters"));
    app.auto.add("mogrt.reset", reset_resp.rect, "Reset All Parameters");
    if reset_resp.clicked() && !mogrt.overrides.is_empty() {
        actions.push(("mogrt.resetControl".into(), json!({"clip": clip.0})));
    }

    if hresp.clicked() && !reset_resp.clicked() {
        toggle_collapsed(&mut app.ui.collapsed_fx, key, open);
    }
    if !open {
        return;
    }

    // Subtitle with capsule name & canvas size
    let def = &mogrt.definition;
    let meta = format!("{}  ({} × {})", def.capsule_name, def.width, def.height);
    let (mr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    ui.painter().text(pos2(mr.min.x + 18.0, mr.center().y), Align2::LEFT_CENTER, meta, Tokens::ui(11.0), t.text_faint);

    let mut ctx = InspectorCtx { app, clip, mogrt, actions };
    ctx.render_controls(ui, &def.controls, 0);

    // Optional After Effects (`aerender`) bridge section when `project.aegraphic` is bundled
    if def.has_aegraphic {
        ctx.render_ae_bridge(ui);
    }
}

fn toggle_collapsed(collapsed: &mut Vec<String>, key: String, currently_open: bool) {
    if currently_open {
        collapsed.push(key);
    } else {
        collapsed.retain(|k| *k != key);
    }
}

fn group_header(ui: &mut egui::Ui, t: &Tokens, title: &str, open: bool, depth: usize) -> bool {
    let indent = 12.0 + (depth as f32) * 10.0;
    ui.add_space(2.0);
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
    icons::paint(
        ui.painter(),
        Rect::from_center_size(pos2(r.min.x + indent, r.center().y), vec2(9.0, 9.0)),
        if open { Icon::ChevronDown } else { Icon::ChevronRight },
        t.text_dim,
    );
    ui.painter().text(pos2(r.min.x + indent + 12.0, r.center().y), Align2::LEFT_CENTER, title, Tokens::semibold(12.0), t.text);
    resp.clicked()
}

fn control_row(ui: &mut egui::Ui, t: &Tokens, label: &str, height: f32, depth: usize) -> egui::Ui {
    let indent = 18.0 + (depth as f32) * 10.0;
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    ui.painter().with_clip_rect(Rect::from_min_max(r.min, pos2((r.min.x + LABEL_W).min(r.max.x), r.max.y))).text(
        pos2(r.min.x + indent, r.center().y),
        Align2::LEFT_CENTER,
        label,
        Tokens::ui(12.0),
        t.text_dim,
    );
    let x0 = (r.min.x + LABEL_W + 4.0).min(r.max.x - 40.0).max(r.min.x);
    ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(pos2(x0, r.min.y + 3.0), pos2((r.max.x - 6.0).max(x0 + 10.0), r.max.y - 3.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    )
}

impl InspectorCtx<'_> {
    fn render_controls(&mut self, ui: &mut egui::Ui, controls: &[MogrtControl], depth: usize) {
        if depth > 8 {
            return;
        }
        let mut flat_group_open = true;
        for ctrl in controls {
            if let MogrtControlKind::Group { children } = &ctrl.kind {
                let group_key = format!("mogrt-grp:{}:{}", self.clip.0, ctrl.id);
                let is_open = !self.app.ui.collapsed_fx.contains(&group_key);
                if group_header(ui, &self.app.tokens, &ctrl.ui_name, is_open, depth) {
                    toggle_collapsed(&mut self.app.ui.collapsed_fx, group_key, is_open);
                }
                if children.is_empty() {
                    flat_group_open = is_open;
                } else if is_open {
                    self.render_controls(ui, children, depth + 1);
                }
                continue;
            }
            if !flat_group_open {
                continue;
            }
            self.render_leaf(ui, ctrl, depth);
        }
    }

    fn render_leaf(&mut self, ui: &mut egui::Ui, ctrl: &MogrtControl, depth: usize) {
        match &ctrl.kind {
            MogrtControlKind::Text { .. } => self.render_text(ui, ctrl, depth),
            MogrtControlKind::Dropdown { options, .. } => self.render_dropdown(ui, ctrl, options, depth),
            MogrtControlKind::Slider { min, max, .. } => self.render_slider(ui, ctrl, (*min, *max), depth),
            MogrtControlKind::Checkbox { .. } => self.render_checkbox(ui, ctrl, depth),
            MogrtControlKind::Color { .. } => self.render_color(ui, ctrl, depth),
            MogrtControlKind::Point { .. } => self.render_point(ui, ctrl, depth),
            MogrtControlKind::StringInfo { text, hidden_by_default } => {
                if !*hidden_by_default && !text.trim().is_empty() {
                    let mut vui = control_row(ui, &self.app.tokens, &ctrl.ui_name, ROW_H, depth);
                    vui.label(egui::RichText::new(text).size(11.5).color(self.app.tokens.text_faint));
                }
            }
            MogrtControlKind::Group { .. } => {}
        }
    }

    fn render_text(&mut self, ui: &mut egui::Ui, ctrl: &MogrtControl, depth: usize) {
        let mut text = match self.mogrt.effective_value(&ctrl.id) {
            Some(MogrtControlValue::Text { text, .. }) => text,
            _ => String::new(),
        };
        let multiline = text.contains('\n');
        let mut vui = control_row(ui, &self.app.tokens, &ctrl.ui_name, if multiline { 48.0 } else { ROW_H }, depth);
        let w = vui.available_width().max(60.0);
        let edit = if multiline {
            egui::TextEdit::multiline(&mut text).desired_rows(2).desired_width(w)
        } else {
            egui::TextEdit::singleline(&mut text).desired_width(w)
        };
        let resp = crate::widgets::accessible_name(vui.add(edit), &ctrl.ui_name);
        self.app.auto.add(&format!("mogrt.control.{}", ctrl.id), resp.rect, &ctrl.ui_name);
        if resp.changed() {
            self.actions.push(("mogrt.setControl".into(), json!({"clip": self.clip.0, "control": ctrl.id, "value": text, "merge": true})));
        }
    }

    fn render_dropdown(&mut self, ui: &mut egui::Ui, ctrl: &MogrtControl, options: &[MogrtMenuOption], depth: usize) {
        let cur = match self.mogrt.effective_value(&ctrl.id) {
            Some(MogrtControlValue::Dropdown(i)) => i,
            _ => 0,
        };
        let mut sel = cur;
        let mut vui = control_row(ui, &self.app.tokens, &ctrl.ui_name, ROW_H, depth);
        let w = vui.available_width().clamp(80.0, 220.0);
        let label = options.get(cur).map_or("", |o| o.name.as_str());
        let combo = craft_ui::ComboBox::from_id_salt(("mogrt-dd", self.clip.0, ctrl.id.as_str())).selected_text(label).width(w).show_ui(&mut vui, |ui| {
            for (i, opt) in options.iter().enumerate() {
                ui.selectable_value(&mut sel, i, opt.name.as_str());
            }
        });
        let resp = crate::widgets::accessible_name(combo.response, &ctrl.ui_name);
        self.app.auto.add(&format!("mogrt.control.{}", ctrl.id), resp.rect, &ctrl.ui_name);
        if sel != cur {
            self.actions.push(("mogrt.setControl".into(), json!({"clip": self.clip.0, "control": ctrl.id, "value": sel})));
        }
    }

    fn render_slider(&mut self, ui: &mut egui::Ui, ctrl: &MogrtControl, bounds: (Option<f64>, Option<f64>), depth: usize) {
        let cur = match self.mogrt.effective_value(&ctrl.id) {
            Some(MogrtControlValue::Number(v)) => v,
            _ => 0.0,
        };
        let mut vui = control_row(ui, &self.app.tokens, &ctrl.ui_name, ROW_H, depth);
        if let (Some(lo), Some(hi)) = bounds
            && lo < hi
        {
            let mut val = cur;
            let resp = crate::widgets::accessible_name(vui.add(egui::Slider::new(&mut val, lo..=hi)), &ctrl.ui_name);
            self.app.auto.add(&format!("mogrt.control.{}", ctrl.id), resp.rect, &ctrl.ui_name);
            if resp.changed() {
                self.actions.push((
                    "mogrt.setControl".into(),
                    json!({"clip": self.clip.0, "control": ctrl.id, "value": val, "merge": true, "begin": resp.drag_started()}),
                ));
            }
        } else {
            let range = (bounds.0.unwrap_or(-100_000.0), bounds.1.unwrap_or(100_000.0));
            let (resp, next) = crate::widgets::hot_number(
                &mut vui,
                &ctrl.ui_name,
                egui::Id::unique(("mogrt-num", self.clip.0, ctrl.id.as_str())),
                cur,
                HotNumberOptions::new(0.5, range, 1, "", &self.app.tokens),
            );
            self.app.auto.add(&format!("mogrt.control.{}", ctrl.id), resp.rect, &ctrl.ui_name);
            if let Some(nv) = next {
                self.actions.push(("mogrt.setControl".into(), json!({"clip": self.clip.0, "control": ctrl.id, "value": nv, "merge": true})));
            }
        }
    }

    fn render_checkbox(&mut self, ui: &mut egui::Ui, ctrl: &MogrtControl, depth: usize) {
        let mut checked = match self.mogrt.effective_value(&ctrl.id) {
            Some(MogrtControlValue::Bool(b)) => b,
            _ => false,
        };
        let mut vui = control_row(ui, &self.app.tokens, &ctrl.ui_name, ROW_H, depth);
        let resp = crate::widgets::accessible_name(vui.checkbox(&mut checked, ""), &ctrl.ui_name);
        self.app.auto.add(&format!("mogrt.control.{}", ctrl.id), resp.rect, &ctrl.ui_name);
        if resp.changed() {
            self.actions.push(("mogrt.setControl".into(), json!({"clip": self.clip.0, "control": ctrl.id, "value": checked})));
        }
    }

    fn render_color(&mut self, ui: &mut egui::Ui, ctrl: &MogrtControl, depth: usize) {
        let c = match self.mogrt.effective_value(&ctrl.id) {
            Some(MogrtControlValue::Color(rgba)) => rgba,
            _ => [1.0, 1.0, 1.0, 1.0],
        };
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let mut c32 = Color32::from_rgba_unmultiplied(q(c[0]), q(c[1]), q(c[2]), q(c[3]));
        let mut vui = control_row(ui, &self.app.tokens, &ctrl.ui_name, ROW_H, depth);
        let resp = egui::color_picker::color_edit_button_srgba(&mut vui, &mut c32, egui::color_picker::Alpha::OnlyBlend);
        self.app.auto.add(&format!("mogrt.control.{}", ctrl.id), resp.rect, &ctrl.ui_name);
        if resp.changed() {
            let [r, g, b, a] = c32.to_srgba_unmultiplied();
            let next = [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, f32::from(a) / 255.0];
            self.actions.push(("mogrt.setControl".into(), json!({"clip": self.clip.0, "control": ctrl.id, "value": next, "merge": true})));
        }
    }

    fn render_point(&mut self, ui: &mut egui::Ui, ctrl: &MogrtControl, depth: usize) {
        let pt = match self.mogrt.effective_value(&ctrl.id) {
            Some(MogrtControlValue::Point(p)) => p,
            _ => [0.0, 0.0],
        };
        let mut vui = control_row(ui, &self.app.tokens, &ctrl.ui_name, ROW_H, depth);
        let (rx, nx) = crate::widgets::hot_number(
            &mut vui,
            &format!("{} X", ctrl.ui_name),
            egui::Id::unique(("mogrt-pt", self.clip.0, ctrl.id.as_str(), "x")),
            pt[0],
            HotNumberOptions::new(1.0, (-100_000.0, 100_000.0), 1, " X", &self.app.tokens),
        );
        vui.add_space(10.0);
        let (ry, ny) = crate::widgets::hot_number(
            &mut vui,
            &format!("{} Y", ctrl.ui_name),
            egui::Id::unique(("mogrt-pt", self.clip.0, ctrl.id.as_str(), "y")),
            pt[1],
            HotNumberOptions::new(1.0, (-100_000.0, 100_000.0), 1, " Y", &self.app.tokens),
        );
        self.app.auto.add(&format!("mogrt.control.{}.x", ctrl.id), rx.rect, &format!("{} X", ctrl.ui_name));
        self.app.auto.add(&format!("mogrt.control.{}.y", ctrl.id), ry.rect, &format!("{} Y", ctrl.ui_name));
        if nx.is_some() || ny.is_some() {
            self.actions.push((
                "mogrt.setControl".into(),
                json!({"clip": self.clip.0, "control": ctrl.id, "value": {"x": nx.unwrap_or(pt[0]), "y": ny.unwrap_or(pt[1])}, "merge": true}),
            ));
        }
    }

    fn render_ae_bridge(&mut self, ui: &mut egui::Ui) {
        let t = self.app.tokens;
        ui.add_space(6.0);
        if filmcraft_engine::mogrt::aerender_available() {
            let (br, bresp) = ui.allocate_exact_size(vec2(ui.available_width().min(250.0), 26.0), Sense::click());
            ui.painter().rect_filled(br, 4.0, if bresp.hovered() { t.hover } else { t.panel_bg });
            ui.painter().rect_stroke(br, 4.0, Stroke::new(1.0, t.separator), StrokeKind::Inside);
            ui.painter().text(br.center(), Align2::CENTER_CENTER, "Render via After Effects (aerender)", Tokens::ui(11.5), t.text);
            self.app.auto.add("mogrt.renderAe", br, "Render via After Effects (aerender)");
            if bresp.clicked() {
                self.actions.push(("mogrt.renderAe".into(), json!({"clip": self.clip.0})));
            }
        } else if self.mogrt.baked_cache_path.is_none() {
            let (lr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
            ui.painter().text(
                pos2(lr.min.x + 18.0, lr.center().y),
                Align2::LEFT_CENTER,
                "Native renderer active (aerender not found)",
                Tokens::ui(11.0),
                t.text_faint,
            );
        }

        if self.mogrt.baked_cache_path.is_some() {
            ui.add_space(4.0);
            let (cr, cresp) = ui.allocate_exact_size(vec2(ui.available_width().min(180.0), 24.0), Sense::click());
            ui.painter().rect_filled(cr, 4.0, if cresp.hovered() { t.hover } else { t.panel_bg });
            ui.painter().rect_stroke(cr, 4.0, Stroke::new(1.0, t.separator), StrokeKind::Inside);
            ui.painter().text(cr.center(), Align2::CENTER_CENTER, "Use Native Renderer", Tokens::ui(11.5), t.text_dim);
            self.app.auto.add("mogrt.clearAeCache", cr, "Use Native Renderer");
            if cresp.clicked() {
                self.actions.push(("mogrt.clearAeCache".into(), json!({"clip": self.clip.0})));
            }
        }
    }
}
