//! Real-time native renderer for Motion Graphics Template (`.mogrt`) clips.
//!
//! Renders `.mogrt` text templates directly with alpha transparency using `filmcraft-text`
//! vector rasterisation, applying per-clip overrides (`Source Text`, `Text Color`, `Text Size`,
//! `Text Position`, `Text Opacity`, tracking, background) and smooth keyframed interpolation for
//! common motion presets (`Slide Up`, `Smooth Zoom`, `Smooth Tracking`, `Smooth Blur`,
//! `Smooth Fade`) scaled by `Animation Speed (In/Out)`.
//!
//! A template with no text controls renders nothing (its name is never drawn as a stand-in);
//! the Media Browser thumbnail still shows its look, and full After Effects graphics need the
//! `aerender` bake.

use filmcraft_geom::{Affine, Vec2};
use filmcraft_project::TrackItem;
use filmcraft_project::graphic::{Appearance, LayerContent, LayerSpec, LayerTransform, TextProps};
use filmcraft_project::mogrt::{MogrtClipState, MogrtControl, MogrtControlKind, MogrtControlValue, MogrtFontValue};
use filmcraft_time::Tick;

use crate::graphic_clip::{LayerRaster, composite_raster, layer_matrix, raster_layer, text_layout, xform};
use crate::image::Image;

/// Built-in animation presets recognised from `.mogrt` dropdown controls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnimationPreset {
    #[default]
    None,
    SlideUp,
    SmoothZoom,
    SmoothTracking,
    SmoothBlur,
    SmoothFade,
}

impl AnimationPreset {
    /// Parse a preset from a dropdown option label (case-insensitive).
    pub fn from_label(label: &str) -> Self {
        let low = label.trim().to_ascii_lowercase();
        if low.is_empty() || low == "none" || low == "off" || low == "static" {
            return Self::None;
        }
        if low.contains("slide") {
            return Self::SlideUp;
        }
        if low.contains("zoom") || low.contains("scale") {
            return Self::SmoothZoom;
        }
        if low.contains("track") || low.contains("spacing") {
            return Self::SmoothTracking;
        }
        if low.contains("blur") {
            return Self::SmoothBlur;
        }
        if low.contains("fade") || low.contains("opacity") {
            return Self::SmoothFade;
        }
        Self::None
    }
}

/// Animated transform and filter offsets evaluated at a single frame time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimState {
    pub offset_y: f64,
    pub scale: f64,
    pub tracking_delta: f32,
    pub blur_sigma: f32,
    pub opacity: f32,
}

impl Default for AnimState {
    fn default() -> Self {
        Self { offset_y: 0.0, scale: 1.0, tracking_delta: 0.0, blur_sigma: 0.0, opacity: 1.0 }
    }
}

/// One resolved text line/block extracted from a `.mogrt` clip state.
#[derive(Clone, Debug)]
struct ResolvedText {
    text: String,
    font: MogrtFontValue,
}

/// Effective rendering parameters resolved from a [`MogrtClipState`].
#[derive(Clone, Debug)]
pub struct ResolvedMogrt {
    texts: Vec<ResolvedText>,
    pub color: [f32; 4],
    pub size: f32,
    pub position: [f64; 2],
    pub opacity: f32,
    pub tracking: f32,
    pub background: Option<([f32; 4], f32, f32)>,
    pub preset_in: AnimationPreset,
    pub preset_out: AnimationPreset,
    pub duration_in_secs: f64,
    pub duration_out_secs: f64,
}

fn ease_out_cubic(t: f64) -> f64 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

fn ease_in_cubic(t: f64) -> f64 {
    let u = t.clamp(0.0, 1.0);
    u * u * u
}

fn speed_to_duration_secs(value: f64, max: Option<f64>) -> f64 {
    const BASE_SECS: f64 = 0.75;
    if !value.is_finite() || value <= 0.0 {
        return BASE_SECS;
    }
    let mult = if max.is_some_and(|m| m > 10.0) || value > 10.0 {
        // Percentage-style slider (e.g. 0..100 where 50 is 1x, 100 is 2x).
        (value / 50.0).clamp(0.2, 5.0)
    } else {
        value.clamp(0.1, 10.0)
    };
    (BASE_SECS / mult).clamp(0.05, 4.0)
}

fn name_matches(ctrl: &MogrtControl, needles: &[&str]) -> bool {
    let name = ctrl.ui_name.to_ascii_lowercase();
    let id = ctrl.id.to_ascii_lowercase();
    needles.iter().any(|n| name.contains(n) || id.contains(n))
}

fn effective_number(state: &MogrtClipState, ctrl: &MogrtControl) -> Option<(f64, Option<f64>)> {
    let max = match &ctrl.kind {
        MogrtControlKind::Slider { max, .. } => *max,
        _ => return None,
    };
    match state.effective_value(&ctrl.id) {
        Some(MogrtControlValue::Number(n)) if n.is_finite() => Some((n, max)),
        _ => None,
    }
}

fn effective_dropdown_label(state: &MogrtClipState, ctrl: &MogrtControl) -> Option<String> {
    let MogrtControlKind::Dropdown { options, .. } = &ctrl.kind else { return None };
    let Some(MogrtControlValue::Dropdown(idx)) = state.effective_value(&ctrl.id) else { return None };
    options.get(idx).map(|o| o.name.clone())
}

struct Resolver {
    cw: f64,
    ch: f64,
    texts: Vec<ResolvedText>,
    color: [f32; 4],
    color_found: bool,
    size: Option<f32>,
    position: [f64; 2],
    pos_found: bool,
    opacity: f32,
    tracking: Option<f32>,
    bg_color: Option<[f32; 4]>,
    bg_enabled: bool,
    bg_opacity: f32,
    preset_in: Option<AnimationPreset>,
    preset_out: Option<AnimationPreset>,
    preset_any: Option<AnimationPreset>,
    dur_in: Option<f64>,
    dur_out: Option<f64>,
    dur_any: Option<f64>,
}

impl Resolver {
    fn new(canvas: (u32, u32)) -> Self {
        let cw = f64::from(canvas.0.max(1));
        let ch = f64::from(canvas.1.max(1));
        Self {
            cw,
            ch,
            texts: Vec::new(),
            color: [1.0, 1.0, 1.0, 1.0],
            color_found: false,
            size: None,
            position: [cw * 0.5, ch * 0.5],
            pos_found: false,
            opacity: 1.0,
            tracking: None,
            bg_color: None,
            bg_enabled: true,
            bg_opacity: 0.85,
            preset_in: None,
            preset_out: None,
            preset_any: None,
            dur_in: None,
            dur_out: None,
            dur_any: None,
        }
    }

    fn apply_control(&mut self, state: &MogrtClipState, ctrl: &MogrtControl) {
        match &ctrl.kind {
            MogrtControlKind::Text { font, .. } => self.apply_text(state, ctrl, font),
            MogrtControlKind::Color { .. } => self.apply_color(state, ctrl),
            MogrtControlKind::Point { default_point } => self.apply_point(state, ctrl, *default_point),
            MogrtControlKind::Slider { .. } => self.apply_slider(state, ctrl),
            MogrtControlKind::Checkbox { .. } => self.apply_checkbox(state, ctrl),
            MogrtControlKind::Dropdown { .. } => self.apply_dropdown(state, ctrl),
            MogrtControlKind::Group { .. } | MogrtControlKind::StringInfo { .. } => {}
        }
    }

    fn apply_text(&mut self, state: &MogrtClipState, ctrl: &MogrtControl, def_font: &MogrtFontValue) {
        let Some(MogrtControlValue::Text { text, font }) = state.effective_value(&ctrl.id) else { return };
        let f = font.unwrap_or_else(|| def_font.clone());
        if self.size.is_none() {
            self.size = f.font_size;
        }
        if self.tracking.is_none() {
            self.tracking = f.tracking;
        }
        self.texts.push(ResolvedText { text, font: f });
    }

    fn apply_color(&mut self, state: &MogrtClipState, ctrl: &MogrtControl) {
        let Some(MogrtControlValue::Color(c)) = state.effective_value(&ctrl.id) else { return };
        if name_matches(ctrl, &["background", "bg", "box", "plate", "shape"]) {
            self.bg_color = Some(c);
        } else if !self.color_found || name_matches(ctrl, &["text", "fill", "font"]) {
            self.color = c;
            self.color_found = true;
        }
    }

    fn apply_point(&mut self, state: &MogrtClipState, ctrl: &MogrtControl, default_point: [f64; 2]) {
        let Some(MogrtControlValue::Point(pt)) = state.effective_value(&ctrl.id) else { return };
        if self.pos_found && !name_matches(ctrl, &["text", "position", "pos"]) {
            return;
        }
        if !state.is_overridden(&ctrl.id) && default_point == [0.0, 0.0] {
            return;
        }
        self.position = normalize_point(pt, self.cw, self.ch);
        self.pos_found = true;
    }

    fn apply_slider(&mut self, state: &MogrtClipState, ctrl: &MogrtControl) {
        let Some((v, max)) = effective_number(state, ctrl) else { return };
        if name_matches(ctrl, &["size", "font size"]) && !name_matches(ctrl, &["box", "bg", "background", "stroke"]) {
            self.size = Some((v as f32).clamp(8.0, 600.0));
        } else if name_matches(ctrl, &["opacity"]) {
            let norm = if max.is_some_and(|m| m <= 1.0) { v as f32 } else { (v / 100.0) as f32 };
            if name_matches(ctrl, &["background", "bg", "box"]) {
                self.bg_opacity = norm.clamp(0.0, 1.0);
            } else {
                self.opacity = norm.clamp(0.0, 1.0);
            }
        } else if name_matches(ctrl, &["tracking", "letter spacing", "spacing"]) {
            self.tracking = Some((v as f32).clamp(-500.0, 1500.0));
        } else if name_matches(ctrl, &["speed", "duration"]) {
            let d = speed_to_duration_secs(v, max);
            let is_in = name_matches(ctrl, &["in", "intro", "enter"]);
            let is_out = name_matches(ctrl, &["out", "outro", "exit"]);
            if is_in && !is_out {
                self.dur_in = Some(d);
            } else if is_out && !is_in {
                self.dur_out = Some(d);
            } else {
                self.dur_any = Some(d);
            }
        }
    }

    fn apply_checkbox(&mut self, state: &MogrtClipState, ctrl: &MogrtControl) {
        if name_matches(ctrl, &["background", "bg", "box"])
            && let Some(MogrtControlValue::Bool(b)) = state.effective_value(&ctrl.id)
        {
            self.bg_enabled = b;
        }
    }

    fn apply_dropdown(&mut self, state: &MogrtClipState, ctrl: &MogrtControl) {
        let Some(label) = effective_dropdown_label(state, ctrl) else { return };
        let preset = AnimationPreset::from_label(&label);
        let is_in = name_matches(ctrl, &["in", "intro", "enter"]);
        let is_out = name_matches(ctrl, &["out", "outro", "exit"]);
        if is_in && !is_out {
            self.preset_in = Some(preset);
        } else if is_out && !is_in {
            self.preset_out = Some(preset);
        } else if name_matches(ctrl, &["anim", "preset", "motion", "style", "effect"]) || preset != AnimationPreset::None {
            self.preset_any = Some(preset);
        }
    }

    fn finish(self, _capsule_name: &str) -> ResolvedMogrt {
        // A template without text controls renders nothing rather than drawing its own name:
        // the Media Browser thumbnail still shows its look, and the After Effects bake covers
        // full graphics.
        let background = self.bg_color.filter(|_| self.bg_enabled).map(|mut c| {
            c[3] = (c[3] * self.bg_opacity).clamp(0.0, 1.0);
            (c, 18.0, 8.0)
        });
        ResolvedMogrt {
            texts: self.texts,
            color: self.color,
            size: self.size.unwrap_or(72.0).clamp(8.0, 600.0),
            position: self.position,
            opacity: self.opacity.clamp(0.0, 1.0),
            tracking: self.tracking.unwrap_or(0.0).clamp(-500.0, 1500.0),
            background,
            preset_in: self.preset_in.or(self.preset_any).unwrap_or(AnimationPreset::None),
            preset_out: self.preset_out.or(self.preset_any).unwrap_or(AnimationPreset::None),
            duration_in_secs: self.dur_in.or(self.dur_any).unwrap_or(0.75),
            duration_out_secs: self.dur_out.or(self.dur_any).unwrap_or(0.75),
        }
    }
}

/// Resolve all effective visual and animation parameters from `state` for a `canvas` frame size.
pub fn resolve_mogrt(state: &MogrtClipState, canvas: (u32, u32)) -> ResolvedMogrt {
    let mut resolver = Resolver::new(canvas);
    for ctrl in state.definition.flat_controls() {
        resolver.apply_control(state, ctrl);
    }
    resolver.finish(&state.definition.capsule_name)
}

fn normalize_point(pt: [f64; 2], cw: f64, ch: f64) -> [f64; 2] {
    let (x, y) = (pt[0], pt[1]);
    if !x.is_finite() || !y.is_finite() {
        return [cw * 0.5, ch * 0.5];
    }
    if (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y) && (x > 0.0 || y > 0.0) && cw >= 16.0 && ch >= 16.0 { [x * cw, y * ch] } else { [x, y] }
}

/// Evaluate keyframed intro/outro animation state at clip-local time `rel` within `clip_duration`.
pub fn eval_animation(resolved: &ResolvedMogrt, rel: Tick, clip_duration: Tick, canvas_h: f64) -> AnimState {
    let mut anim = AnimState::default();
    let clip_secs = clip_duration.seconds();
    if !clip_secs.is_finite() || clip_secs <= 0.0 {
        return anim;
    }
    let u = rel.seconds().clamp(0.0, clip_secs);
    let half = (clip_secs * 0.5).max(0.001);
    let in_dur = resolved.duration_in_secs.clamp(0.02, half);
    let out_dur = resolved.duration_out_secs.clamp(0.02, half);
    let slide_dist = (canvas_h * 0.08).clamp(24.0, 160.0);

    if resolved.preset_in != AnimationPreset::None && u < in_dur {
        let e = ease_out_cubic(u / in_dur);
        let k = 1.0 - e;
        apply_preset(&mut anim, resolved.preset_in, e, k, slide_dist, true);
    }

    let out_start = (clip_secs - out_dur).max(0.0);
    if resolved.preset_out != AnimationPreset::None && u > out_start {
        let e = ease_in_cubic((u - out_start) / out_dur);
        let keep = 1.0 - e;
        apply_preset(&mut anim, resolved.preset_out, keep, e, slide_dist, false);
    }
    anim
}

fn apply_preset(anim: &mut AnimState, preset: AnimationPreset, vis: f64, delta: f64, slide_dist: f64, is_in: bool) {
    match preset {
        AnimationPreset::None => {}
        AnimationPreset::SlideUp => {
            anim.offset_y += if is_in { slide_dist * delta } else { -slide_dist * delta };
            anim.opacity *= vis as f32;
        }
        AnimationPreset::SmoothZoom => {
            anim.scale *= 0.65 + 0.35 * vis;
            anim.opacity *= vis as f32;
        }
        AnimationPreset::SmoothTracking => {
            anim.tracking_delta += (120.0 * delta) as f32;
            anim.opacity *= vis as f32;
        }
        AnimationPreset::SmoothBlur => {
            anim.blur_sigma = anim.blur_sigma.max((18.0 * delta) as f32);
            anim.opacity *= vis as f32;
        }
        AnimationPreset::SmoothFade => {
            anim.opacity *= vis as f32;
        }
    }
}

fn build_layer_specs(resolved: &ResolvedMogrt, anim: &AnimState) -> Vec<LayerSpec> {
    let opacity = (resolved.opacity * anim.opacity).clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return Vec::new();
    }
    let visible_texts: Vec<&ResolvedText> = resolved.texts.iter().filter(|t| !t.text.is_empty()).collect();
    let count = visible_texts.len();
    let mut specs = Vec::with_capacity(count);

    for (i, rt) in visible_texts.into_iter().enumerate() {
        let font_size = if i == 0 { resolved.size } else { rt.font.font_size.unwrap_or(resolved.size * 0.62).clamp(8.0, 600.0) };
        let caps = if rt.font.all_caps {
            1
        } else if rt.font.small_caps {
            2
        } else {
            0
        };
        let props = TextProps {
            text: rt.text.clone(),
            font: rt.font.font_family.clone(),
            style: rt.font.font_style.clone(),
            size: font_size,
            align: 1, // Center
            tracking: resolved.tracking + anim.tracking_delta,
            kerning: true,
            ligatures: true,
            leading: rt.font.leading.unwrap_or(0.0),
            baseline_shift: 0.0,
            faux_bold: rt.font.faux_bold,
            faux_italic: rt.font.faux_italic,
            caps,
            underline: false,
            strikethrough: false,
            box_width: 0.0,
            box_height: 0.0,
            vertical: false,
            runs: Vec::new(),
        };
        // Centre the laid-out text block around its anchor point so scale & position feel natural.
        let bounds = text_layout(&props).bounds;
        let anchor = Vec2::new(f64::from((bounds[0] + bounds[2]) * 0.5), f64::from((bounds[1] + bounds[3]) * 0.5));
        let line_step = f64::from(resolved.size) * 1.15;
        let stack_y = if count > 1 { (i as f64 - (count as f64 - 1.0) * 0.5) * line_step } else { 0.0 };
        let pos = Vec2::new(resolved.position[0], resolved.position[1] + stack_y + anim.offset_y);
        let scale = anim.scale.clamp(0.05, 10.0);

        specs.push(LayerSpec {
            name: format!("mogrt_{i}"),
            enabled: true,
            uid: (i as u64) + 1,
            pin: None,
            content: LayerContent::Text(props),
            appearance: Appearance {
                fill: Some(resolved.color),
                gradient: None,
                strokes: Vec::new(),
                background: if i == 0 { resolved.background } else { None },
                shadow: None,
            },
            transform: LayerTransform { position: pos, anchor, scale: Vec2::new(scale, scale), rotation: 0.0, opacity },
        });
    }
    specs
}

fn rasterize_with_blur(spec: &LayerSpec, m: &Affine, cw: usize, ch: usize, blur_sigma: f32) -> Option<LayerRaster> {
    let lm = xform(&m.then_apply(&layer_matrix(spec)));
    let mut r = raster_layer(spec, &lm, cw, ch)?;
    if blur_sigma <= 0.25 {
        return Some(r);
    }
    let scale = ((m.a * m.a + m.b * m.b).sqrt()).max((m.c * m.c + m.d * m.d).sqrt()).clamp(0.05, 8.0) as f32;
    let sigma = (blur_sigma * scale).clamp(0.25, 64.0);
    let pad = (sigma * 2.5).ceil() as usize;
    let nw = r.img.w.saturating_add(pad.saturating_mul(2));
    let nh = r.img.h.saturating_add(pad.saturating_mul(2));
    if nw.checked_mul(nh).is_none_or(|px| px > (32 << 20)) {
        return Some(r);
    }
    let mut padded = Image::new(nw, nh);
    for y in 0..r.img.h {
        let src_off = y * r.img.w * 4;
        let dst_off = ((y + pad) * nw + pad) * 4;
        if let (Some(src), Some(dst)) = (r.img.px.get(src_off..src_off + r.img.w * 4), padded.px.get_mut(dst_off..dst_off + r.img.w * 4)) {
            dst.copy_from_slice(src);
        }
    }
    crate::effects::gaussian(&mut padded, sigma, sigma, false);
    r.x = r.x.saturating_sub(pad as i32);
    r.y = r.y.saturating_sub(pad as i32);
    r.img = padded;
    Some(r)
}

/// Render a `.mogrt` clip at timeline time `t` onto `out` using the native fallback renderer.
/// `m` maps template canvas pixels (`0..canvas.0`, `0..canvas.1`) to `out` pixels.
pub fn render_mogrt(mogrt: &MogrtClipState, item: &TrackItem, t: Tick, canvas: (u32, u32), m: &Affine, out: &mut Image) {
    let resolved = resolve_mogrt(mogrt, canvas);
    let anim = eval_animation(&resolved, t - item.start, item.duration, f64::from(canvas.1.max(1)));
    let specs = build_layer_specs(&resolved, &anim);
    for spec in &specs {
        if let Some(r) = rasterize_with_blur(spec, m, out.w, out.h, anim.blur_sigma) {
            composite_raster(out, &r, 0, 0);
        }
    }
}

/// Render a `.mogrt` clip into one tight premultiplied linear image (for the GPU frame plan):
/// returns `(image, x, y)` in output pixels, or `None` when fully transparent.
pub fn render_mogrt_tight(mogrt: &MogrtClipState, item: &TrackItem, t: Tick, canvas: (u32, u32), m: &Affine, w: usize, h: usize) -> Option<(Image, i32, i32)> {
    let resolved = resolve_mogrt(mogrt, canvas);
    let anim = eval_animation(&resolved, t - item.start, item.duration, f64::from(canvas.1.max(1)));
    let specs = build_layer_specs(&resolved, &anim);
    let rasters: Vec<LayerRaster> = specs.iter().filter_map(|s| rasterize_with_blur(s, m, w, h, anim.blur_sigma)).collect();
    let x0 = rasters.iter().map(|r| r.x).min()?.max(0);
    let y0 = rasters.iter().map(|r| r.y).min()?.max(0);
    let x1 = rasters.iter().map(|r| r.x + r.img.w as i32).max()?.min(w as i32);
    let y1 = rasters.iter().map(|r| r.y + r.img.h as i32).max()?.min(h as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let mut img = Image::new((x1 - x0) as usize, (y1 - y0) as usize);
    for r in &rasters {
        composite_raster(&mut img, r, -x0, -y0);
    }
    Some((img, x0, y0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_project::mogrt::{MogrtControl, MogrtControlKind, MogrtDefinition, MogrtFontValue, MogrtMenuOption};
    use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, Project, TrackKind};
    use filmcraft_time::{FrameRate, TICKS_PER_SECOND, TimeRange};

    fn sample_state(preset_name: &str) -> MogrtClipState {
        let def = MogrtDefinition {
            capsule_name: "Animated Title".into(),
            width: 640,
            height: 360,
            duration: Tick(4 * TICKS_PER_SECOND),
            frame_rate: FrameRate::FPS_30,
            controls: vec![
                MogrtControl {
                    id: "text".into(),
                    ui_name: "Source Text".into(),
                    control_type: 6,
                    group: None,
                    kind: MogrtControlKind::Text { default_text: "CADR".into(), font: MogrtFontValue::default() },
                },
                MogrtControl {
                    id: "color".into(),
                    ui_name: "Text Color".into(),
                    control_type: 4,
                    group: None,
                    kind: MogrtControlKind::Color { default_rgba: [1.0, 0.2, 0.1, 1.0] },
                },
                MogrtControl {
                    id: "size".into(),
                    ui_name: "Text Size".into(),
                    control_type: 2,
                    group: None,
                    kind: MogrtControlKind::Slider { default_value: 64.0, min: Some(12.0), max: Some(200.0) },
                },
                MogrtControl {
                    id: "pos".into(),
                    ui_name: "Text Position".into(),
                    control_type: 5,
                    group: None,
                    kind: MogrtControlKind::Point { default_point: [320.0, 180.0] },
                },
                MogrtControl {
                    id: "opacity".into(),
                    ui_name: "Text Opacity".into(),
                    control_type: 2,
                    group: None,
                    kind: MogrtControlKind::Slider { default_value: 100.0, min: Some(0.0), max: Some(100.0) },
                },
                MogrtControl {
                    id: "preset".into(),
                    ui_name: "Animation Preset".into(),
                    control_type: 13,
                    group: None,
                    kind: MogrtControlKind::Dropdown {
                        options: vec![
                            MogrtMenuOption { name: "None".into(), value: 0 },
                            MogrtMenuOption { name: "Slide Up".into(), value: 1 },
                            MogrtMenuOption { name: "Smooth Zoom".into(), value: 2 },
                            MogrtMenuOption { name: "Smooth Tracking".into(), value: 3 },
                            MogrtMenuOption { name: "Smooth Blur".into(), value: 4 },
                            MogrtMenuOption { name: "Smooth Fade".into(), value: 5 },
                        ],
                        default_index: 0,
                    },
                },
                MogrtControl {
                    id: "speed_in".into(),
                    ui_name: "Animation Speed In".into(),
                    control_type: 2,
                    group: None,
                    kind: MogrtControlKind::Slider { default_value: 1.0, min: Some(0.2), max: Some(4.0) },
                },
                MogrtControl {
                    id: "speed_out".into(),
                    ui_name: "Animation Speed Out".into(),
                    control_type: 2,
                    group: None,
                    kind: MogrtControlKind::Slider { default_value: 1.0, min: Some(0.2), max: Some(4.0) },
                },
            ],
            ..MogrtDefinition::default()
        };
        let mut st = MogrtClipState::from_definition(def);
        let idx = match preset_name {
            "Slide Up" => 1,
            "Smooth Zoom" => 2,
            "Smooth Tracking" => 3,
            "Smooth Blur" => 4,
            "Smooth Fade" => 5,
            _ => 0,
        };
        st.set_override("preset", MogrtControlValue::Dropdown(idx)).unwrap();
        st
    }

    fn dummy_item(st: MogrtClipState) -> TrackItem {
        let mut p = Project::new("t");
        let rate = FrameRate::FPS_30;
        let info = filmcraft_media::MediaInfo {
            name: "title.mogrt".into(),
            kind: filmcraft_media::MediaKind::Movie,
            duration: Tick(4 * TICKS_PER_SECOND),
            video: Some(filmcraft_media::VideoStreamInfo {
                width: 640,
                height: 360,
                frame_rate: rate,
                par: (1, 1),
                codec: "MOGRT".into(),
                pixel_format: "RGBA 8-bit".into(),
                color: filmcraft_color::ColorInfo::SRGB_FULL,
                has_alpha: true,
                bitrate: None,
                hdr: None,
                color_assumed: None,
            }),
            audio_streams: Vec::new(),
            container: "MOGRT".into(),
            start_timecode: None,
            file_size: None,
        };
        let item_id = p.add_item(
            "title.mogrt",
            Label::Rose,
            ItemKind::Media(Box::new(MediaClip {
                media: MediaRef::File { path: "title.mogrt".into() },
                info,
                interpret: Default::default(),
                mark_in: None,
                mark_out: None,
                markers: Vec::new(),
                offline: false,
                proxy: None,
                identity: None,
            })),
            None,
        );
        p.mogrt_templates.insert(item_id, st.definition.clone());
        let mut ti = p.make_track_item(item_id, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick(4 * TICKS_PER_SECOND)), rate).unwrap();
        ti.mogrt = Some(Box::new(st));
        ti
    }

    fn count_opaque_pixels(img: &Image) -> usize {
        img.px.as_chunks::<4>().0.iter().filter(|p| p[3] > 0.1).count()
    }

    #[test]
    fn renders_text_with_alpha_and_respects_overrides() {
        let mut st = sample_state("None");
        let item = dummy_item(st.clone());
        let mut img = Image::new(640, 360);
        render_mogrt(&st, &item, Tick(2 * TICKS_PER_SECOND), (640, 360), &Affine::IDENTITY, &mut img);
        let lit = count_opaque_pixels(&img);
        assert!(lit > 100, "expected rendered glyph pixels, got {lit}");
        // Corners remain transparent
        assert_eq!(img.get(0, 0)[3], 0.0);

        // Zero opacity yields completely transparent output
        st.set_override("opacity", MogrtControlValue::Number(0.0)).unwrap();
        let mut hidden = Image::new(640, 360);
        render_mogrt(&st, &item, Tick(2 * TICKS_PER_SECOND), (640, 360), &Affine::IDENTITY, &mut hidden);
        assert_eq!(count_opaque_pixels(&hidden), 0);
    }

    #[test]
    fn template_without_text_controls_renders_nothing() {
        // Graphics-only templates render empty; the template's name is never drawn as a stand-in.
        let mut st = sample_state("None");
        st.definition.controls.retain(|c| c.control_type != 6);
        let item = dummy_item(st.clone());
        let mut img = Image::new(640, 360);
        render_mogrt(&st, &item, Tick(2 * TICKS_PER_SECOND), (640, 360), &Affine::IDENTITY, &mut img);
        assert_eq!(count_opaque_pixels(&img), 0, "no text controls: expected an empty render, not the template name");
        assert!(render_mogrt_tight(&st, &item, Tick(2 * TICKS_PER_SECOND), (640, 360), &Affine::IDENTITY, 640, 360).is_none());
    }

    #[test]
    fn animation_presets_and_speed_interpolate_smoothly() {
        for preset in ["Slide Up", "Smooth Zoom", "Smooth Tracking", "Smooth Blur", "Smooth Fade"] {
            let st = sample_state(preset);
            let item = dummy_item(st.clone());
            let resolved = resolve_mogrt(&st, (640, 360));

            let at_start = eval_animation(&resolved, Tick::ZERO, item.duration, 360.0);
            let at_mid_in = eval_animation(&resolved, Tick::from_seconds_f64(0.35), item.duration, 360.0);
            let at_rest = eval_animation(&resolved, Tick(2 * TICKS_PER_SECOND), item.duration, 360.0);
            let at_end = eval_animation(&resolved, item.duration, item.duration, 360.0);

            assert!(at_start.opacity < 0.05, "{preset}: starts transparent");
            assert!(at_mid_in.opacity > at_start.opacity && at_mid_in.opacity < at_rest.opacity, "{preset}: fades in");
            assert!((at_rest.opacity - 1.0).abs() < 1e-4, "{preset}: reaches full opacity at rest");
            assert!(at_end.opacity < 0.05, "{preset}: fades out at end");

            // Render a mid-intro frame (exercises blur, tracking, zoom, slide, fade)
            let tight = render_mogrt_tight(&st, &item, Tick::from_seconds_f64(0.35), (640, 360), &Affine::IDENTITY, 640, 360);
            assert!(tight.is_some(), "{preset}: mid-intro frame should produce visible pixels");
        }
    }

    #[test]
    fn preset_labels_map_case_and_synonym_forgivingly() {
        assert_eq!(AnimationPreset::from_label("None"), AnimationPreset::None);
        assert_eq!(AnimationPreset::from_label("off"), AnimationPreset::None);
        assert_eq!(AnimationPreset::from_label("  Static  "), AnimationPreset::None);
        assert_eq!(AnimationPreset::from_label(""), AnimationPreset::None);
        assert_eq!(AnimationPreset::from_label("Slide Up"), AnimationPreset::SlideUp);
        assert_eq!(AnimationPreset::from_label("SMOOTH ZOOM"), AnimationPreset::SmoothZoom);
        assert_eq!(AnimationPreset::from_label("Scale Pop"), AnimationPreset::SmoothZoom);
        assert_eq!(AnimationPreset::from_label("Letter Spacing"), AnimationPreset::SmoothTracking);
        assert_eq!(AnimationPreset::from_label("Gaussian Blur"), AnimationPreset::SmoothBlur);
        assert_eq!(AnimationPreset::from_label("Opacity Fade"), AnimationPreset::SmoothFade);
        assert_eq!(AnimationPreset::from_label("Wiggle"), AnimationPreset::None);
    }

    #[test]
    fn render_output_is_deterministic() {
        let st = sample_state("Slide Up");
        let item = dummy_item(st.clone());
        let mut a = Image::new(640, 360);
        let mut b = Image::new(640, 360);
        let t = Tick(2 * TICKS_PER_SECOND);
        render_mogrt(&st, &item, t, (640, 360), &Affine::IDENTITY, &mut a);
        render_mogrt(&st, &item, t, (640, 360), &Affine::IDENTITY, &mut b);
        assert_eq!(a.px, b.px);
    }
}
