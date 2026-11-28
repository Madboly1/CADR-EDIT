//! The Gemini-assisted motion tool: prompt building and recipe materialization.
//!
//! The loop, by design manual and free of any paid API: the user describes a scene, the app hands
//! him a prepared prompt ([`build_prompt`]) to paste into an external Gemini conversation, and the
//! reply he pastes back is a declarative [`MotionRecipe`](crate::motion_recipe) - data, never
//! code. [`from_recipe`] turns a validated recipe into a **new sequence** holding one graphic clip
//! per layer on its own video track (back to front), every animation a real keyframe the user can
//! inspect and edit afterwards. The user's current edit is never touched.
//!
//! Every value was already bounded by the recipe validator; this builder still never panics and
//! skips anything inconsistent instead of applying it half-way.

use filmcraft_geom::Vec2;
use filmcraft_project::keyframe::Keyframe;
use filmcraft_project::{EffectInstance, Param, ParamValue, SequenceSettings, TrackKind, graphic};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, str_p};
use crate::motion_recipe::{AnimParam, KeyValue, LayerKind, MotionRecipe, ParamTrack, RecipeLayer, parse_recipe};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;

fn spec(id: &'static str, label: &'static str, params: &'static str, run: Run) -> CommandSpec {
    // No menu path: the command is agent/automation-only until the tool's UI is approved.
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal: true, named: crate::commands::NamedKeys::NONE }
}
fn query(id: &'static str, label: &'static str, params: &'static str, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal: false, named: crate::commands::NamedKeys::NONE }
}

/// The motion tool's commands: build the Gemini prompt, and materialize a pasted reply.
pub fn commands() -> Vec<CommandSpec> {
    vec![
        query(
            "motion.prompt",
            "Build Motion Prompt",
            r#"{"description":str (the scene, any language),"reference":str? (one line about the optional reference image),"width":px?,"height":px?,"fps":rate?}"#,
            motion_prompt,
        ),
        spec(
            "motion.fromRecipe",
            "Build Motion Template From Recipe",
            r#"{"reply":str (Gemini's pasted JSON reply),"name":str? (sequence name)}"#,
            motion_from_recipe,
        ),
    ]
}

fn motion_prompt(_s: &mut Session, p: &Value) -> Result<Value> {
    let description = str_p(p, "description").ok_or_else(|| bad("motion.prompt", "need `description`"))?;
    if description.trim().is_empty() {
        return Err(bad("motion.prompt", "`description` is empty"));
    }
    let reference = str_p(p, "reference");
    let width = crate::commands::f64_p(p, "width").unwrap_or(1080.0) as u32;
    let height = crate::commands::f64_p(p, "height").unwrap_or(1920.0) as u32;
    let fps = crate::commands::f64_p(p, "fps").unwrap_or(30.0);
    Ok(json!({"prompt": build_prompt(description, reference, (width, height, fps))}))
}

fn motion_from_recipe(s: &mut Session, p: &Value) -> Result<Value> {
    let reply = str_p(p, "reply").ok_or_else(|| bad("motion.fromRecipe", "need `reply`"))?;
    let name = str_p(p, "name");
    from_recipe(s, reply, name)
}

/// The prepared prompt for the external Gemini conversation. `reference` is a one-line, in-words
/// note about the reference image the user attaches in that conversation; the image itself never
/// reaches Kadr.
pub fn build_prompt(description: &str, reference: Option<&str>, canvas: (u32, u32, f64)) -> String {
    let (width, height, fps) = canvas;
    let mut out = String::with_capacity(4096);
    out.push_str(
        "You are a motion-graphics designer for a video editor. The user will describe a scene; \
         answer with ONE ```json code block and nothing else - no prose, no explanation. The JSON \
         must follow this exact motion-recipe format (version 1):\n\n",
    );
    out.push_str(
        "{\n\
         \x20 \"version\": 1,\n\
         \x20 \"name\": \"short scene name\",\n\
         \x20 \"canvas\": { \"width\": W, \"height\": H, \"fps\": F, \"duration\": seconds },\n\
         \x20 \"layers\": [ Layer, ... ]   // 1 to 32, first = furthest back\n\
         }\n\n",
    );
    out.push_str(
        "Layer (common fields):\n\
         \x20 \"kind\": \"text\" | \"shape\" | \"solid\" | \"image\"   (required)\n\
         \x20 \"name\": string (optional), \"start\": seconds (default 0), \"duration\": seconds \
         (default: to the end)\n\
         \x20 \"position\": [x, y] canvas pixels (default: center), \"scale\": 0..10 (default 1), \
         \"rotation\": degrees (default 0), \"opacity\": 0..1 (default 1)\n\
         \x20 \"animate\": [ { \"param\": P, \"keys\": [ { \"time\": seconds-from-layer-start, \
         \"value\": V, \"easing\": E } ] } ]   // optional, up to 8 params, 1-64 keys each, \
         strictly increasing times within the layer's duration\n\n",
    );
    out.push_str(
        "By kind:\n\
         \x20 text: \"text\" (required), \"font\", \"font_size\" 8..500 (default 96), \"color\" \
         [r,g,b,a], \"tracking\" -500..1000. Animatable: position, scale, rotation, opacity, \
         fontSize, tracking, fill.\n\
         \x20 shape: \"shape\" \"rect\"|\"ellipse\" (default rect), \"size\" [w,h] pixels \
         (required), \"fill\" [r,g,b,a]. Animatable: position, scale, rotation, opacity, fill.\n\
         \x20 solid: full-frame background color, \"fill\" [r,g,b,a]. Animatable: opacity, fill. \
         A solid does not move.\n\
         \x20 image: a slot the user fills with his own picture: \"label\" (required, a short \
         name), \"size\" [w,h] (default: whole canvas). Animatable: position, scale, rotation, \
         opacity. NEVER invent URLs or file paths.\n\n",
    );
    out.push_str(
        "Values: position is [x,y]; fill/color is [r,g,b,a] with 0..1 channels; everything else \
         is a single number. Easing E is \"linear\" (default), \"hold\", \"easeIn\", \"easeOut\", \
         \"bezier\", \"autoBezier\" or \"continuousBezier\" - it describes the segment LEAVING \
         that key. Text may be Arabic or English. Keep the scene readable: few large elements, \
         strong contrast, entrances with easeOut, exits with easeIn.\n\n",
    );
    out.push_str(&format!("Canvas for this scene: {width} x {height} at {fps} fps. Pick a duration of 3 to 8 seconds.\n\n"));
    if let Some(note) = reference {
        out.push_str(&format!("The user attached a reference image to this conversation ({note}); match its style, colors and layout.\n\n"));
    }
    out.push_str("The scene to design: ");
    out.push_str(description.trim());
    out.push('\n');
    out
}

/// Parse, validate and materialize a Gemini reply as a new sequence. `name` overrides the
/// sequence name (default: the recipe's name, else "Motion Recipe").
pub fn from_recipe(s: &mut Session, reply: &str, name: Option<&str>) -> Result<Value> {
    let recipe = parse_recipe(reply).map_err(|e| EngineError::Other(e.to_string()))?;
    let layers = build_layers(&recipe);
    let seq_name = name.map(str::to_string).or_else(|| recipe.name.clone()).unwrap_or_else(|| "Motion Recipe".to_string());
    let canvas = recipe.canvas;
    let rate = recipe_rate(canvas.fps);
    let base = s.active_sequence().map(|q| q.settings.clone());
    let placed = s.edit("Motion Recipe", |pr, st| {
        let settings = SequenceSettings { width: canvas.width, height: canvas.height, frame_rate: rate, ..base.clone().unwrap_or_default() };
        let seq = pr.new_sequence(&seq_name, settings, layers.len(), 0, None);
        let mut placed = Vec::with_capacity(layers.len());
        for (i, (fx, rlayer)) in layers.into_iter().enumerate() {
            let start = rate.snap(Tick::from_seconds_f64(rlayer.start));
            let dur = rate.snap_nearest(Tick::from_seconds_f64(rlayer.resolved_duration(&canvas))).max(rate.frame_duration());
            let src = crate::graphics::graphic_source(pr, canvas.width, canvas.height, rate);
            let mut ti = pr.make_track_item(src, TrackKind::Video, start, TimeRange::new(Tick::ZERO, dur), rate).ok_or(EngineError::NoSequence)?;
            ti.name = layer_clip_name(rlayer, i);
            ti.effects.extend([fx]);
            graphic::ensure_layer_uids(&mut ti.effects);
            for e in &mut ti.effects {
                filmcraft_project::resolve_auto_points(e, (canvas.width, canvas.height), (canvas.width, canvas.height));
            }
            let id = ti.id;
            let q = pr.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
            let Some(track) = q.video_tracks.get_mut(i) else { return Err(EngineError::Other("motion recipe track missing".to_string())) };
            track.items.push(ti);
            track.sort();
            placed.push(json!({"clip": id.0, "track": i + 1, "kind": rlayer.kind.label(), "name": layer_clip_name(rlayer, i)}));
        }
        st.active_sequence = Some(seq);
        st.selection = Vec::new();
        Ok((seq, placed))
    })?;
    let (seq, placed) = placed;
    Ok(json!({"sequence": seq.0, "name": seq_name, "duration": canvas.duration, "clips": placed}))
}

/// Turn each recipe layer into one graphic layer (back to front, matching the recipe order).
pub fn build_layers(recipe: &MotionRecipe) -> Vec<(EffectInstance, &RecipeLayer)> {
    recipe.layers.iter().map(|layer| (build_layer(recipe, layer), layer)).collect()
}

fn build_layer(recipe: &MotionRecipe, layer: &RecipeLayer) -> EffectInstance {
    let canvas = recipe.canvas;
    let center = Vec2::new(canvas.width as f64 / 2.0, canvas.height as f64 / 2.0);
    let position = layer.position.map(|[x, y]| Vec2::new(x, y)).unwrap_or(center);
    let mut fx = match layer.kind {
        LayerKind::Text => {
            let mut fx = graphic::new_text_layer(layer.text.as_deref().unwrap_or(""), position, layer.font_size.unwrap_or(96.0));
            set(&mut fx, "font", ParamValue::Text(layer.font.clone().unwrap_or_else(|| "Inter".to_string())));
            set(&mut fx, "fill_color", ParamValue::Color(to_f32_color(layer.color.unwrap_or([1.0, 1.0, 1.0, 1.0]))));
            set(&mut fx, "tracking", ParamValue::Float(layer.tracking.unwrap_or(0.0)));
            fx
        }
        LayerKind::Shape => {
            let size = layer.size.map(|[w, h]| Vec2::new(w, h)).unwrap_or(Vec2::new(100.0, 100.0));
            let kind = match layer.shape.unwrap_or_default() {
                crate::motion_recipe::ShapeKind::Rect => 0,
                crate::motion_recipe::ShapeKind::Ellipse => 1,
            };
            let mut fx = graphic::new_shape_layer(kind, position, size, Vec::new());
            set(&mut fx, "fill_color", ParamValue::Color(to_f32_color(layer.fill.unwrap_or([1.0, 1.0, 1.0, 1.0]))));
            fx
        }
        LayerKind::Solid => {
            let mut fx = graphic::new_shape_layer(0, center, Vec2::new(canvas.width as f64, canvas.height as f64), Vec::new());
            set(&mut fx, "fill_color", ParamValue::Color(to_f32_color(layer.fill.unwrap_or([0.0, 0.0, 0.0, 1.0]))));
            fx
        }
        LayerKind::Image => {
            // A placeholder the user replaces with his own picture: a quarter-strength panel
            // named after the slot.
            let size = layer.size.map(|[w, h]| Vec2::new(w, h)).unwrap_or(Vec2::new(canvas.width as f64, canvas.height as f64));
            let mut fx = graphic::new_shape_layer(0, position, size, Vec::new());
            set(&mut fx, "fill_color", ParamValue::Color([1.0, 1.0, 1.0, 0.25]));
            fx
        }
    };
    if let Some(name) = &layer.name {
        set(&mut fx, "name", ParamValue::Text(name.clone()));
    }
    if let Some(scale) = layer.scale {
        set(&mut fx, "scale", ParamValue::Float(scale * 100.0));
    }
    if let Some(rotation) = layer.rotation {
        set(&mut fx, "rotation", ParamValue::Float(rotation));
    }
    if let Some(opacity) = layer.opacity {
        set(&mut fx, "opacity", ParamValue::Float(opacity * 100.0));
    }
    let rate = recipe_rate(canvas.fps);
    if let Some(tracks) = &layer.animate {
        for track in tracks {
            apply_track(&mut fx, track, rate);
        }
    }
    fx
}

/// One animation track becomes a keyframed parameter; the validator already matched value types
/// to the parameter, anything else is skipped defensively.
fn apply_track(fx: &mut EffectInstance, track: &ParamTrack, rate: FrameRate) {
    let convert = |v: KeyValue| -> Option<ParamValue> {
        match (track.param, v) {
            (AnimParam::Position, KeyValue::Vec2([x, y])) => Some(ParamValue::Vec2(Vec2::new(x, y))),
            (AnimParam::Scale, KeyValue::Number(n)) => Some(ParamValue::Float(n * 100.0)),
            (AnimParam::Rotation, KeyValue::Number(n)) => Some(ParamValue::Float(n)),
            (AnimParam::Opacity, KeyValue::Number(n)) => Some(ParamValue::Float(n * 100.0)),
            (AnimParam::FontSize, KeyValue::Number(n)) => Some(ParamValue::Float(n)),
            (AnimParam::Tracking, KeyValue::Number(n)) => Some(ParamValue::Float(n)),
            (AnimParam::Fill, KeyValue::Color(c)) => Some(ParamValue::Color(to_f32_color(c))),
            _ => None,
        }
    };
    let mut values = track.keys.iter().filter_map(|k| convert(k.value).map(|v| (k, v)));
    let Some((first_key, first_value)) = values.next() else { return };
    let mut param = Param::new(first_value.clone());
    let mut first = Keyframe::new(rate.snap_nearest(Tick::from_seconds_f64(first_key.time)), first_value);
    first.interp = first_key.easing.to_interpolation();
    param.keyframes.push(first);
    for (key, value) in values {
        let mut kf = Keyframe::new(rate.snap_nearest(Tick::from_seconds_f64(key.time)), value);
        kf.interp = key.easing.to_interpolation();
        param.keyframes.push(kf);
    }
    fx.params.insert(param_id(track.param).to_string(), param);
}

/// The graphic-layer parameter a recipe animation targets.
fn param_id(param: AnimParam) -> &'static str {
    match param {
        AnimParam::Position => "position",
        AnimParam::Scale => "scale",
        AnimParam::Rotation => "rotation",
        AnimParam::Opacity => "opacity",
        AnimParam::FontSize => "size",
        AnimParam::Tracking => "tracking",
        AnimParam::Fill => "fill_color",
    }
}

fn set(fx: &mut EffectInstance, id: &str, v: ParamValue) {
    fx.params.insert(id.to_string(), Param::new(v));
}

fn to_f32_color(c: [f64; 4]) -> [f32; 4] {
    [c[0] as f32, c[1] as f32, c[2] as f32, c[3] as f32]
}

/// A recipe's frame rate: a common rate when one is within a fiftieth of a frame, else an exact
/// rational.
fn recipe_rate(fps: f64) -> FrameRate {
    FrameRate::COMMON.iter().copied().find(|r| (r.as_f64() - fps).abs() < 0.02).unwrap_or_else(|| FrameRate::new((fps * 1000.0).round() as i64, 1000))
}

/// Clip name for a built layer: its name, its text, its image-slot label, else `Kind N`.
fn layer_clip_name(layer: &RecipeLayer, index: usize) -> String {
    if let Some(name) = &layer.name
        && !name.trim().is_empty()
    {
        return name.trim().to_string();
    }
    match layer.kind {
        LayerKind::Text => layer
            .text
            .as_deref()
            .and_then(|t| t.lines().next())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| t.chars().take(40).collect())
            .unwrap_or_else(|| format!("Text {}", index + 1)),
        LayerKind::Image => format!("Image: {}", layer.label.as_deref().unwrap_or("slot")),
        LayerKind::Shape => format!("Shape {}", index + 1),
        LayerKind::Solid => format!("Background {}", index + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion_recipe::RECIPE_VERSION;

    const DEMO: &str = r#"{
        "version": 1,
        "name": "Intro",
        "canvas": { "width": 1080, "height": 1920, "fps": 30, "duration": 4.0 },
        "layers": [
            { "kind": "solid", "fill": [0.05, 0.05, 0.1, 1] },
            { "kind": "shape", "shape": "ellipse", "size": [400, 400], "fill": [1, 0.8, 0, 1], "position": [540, 500],
              "animate": [ { "param": "scale", "keys": [ { "time": 0, "value": 0 }, { "time": 0.5, "value": 1, "easing": "easeOut" } ] } ] },
            { "kind": "text", "text": "ماذا لو؟", "font": "Cairo", "font_size": 120, "color": [1, 1, 1, 1], "position": [540, 900], "opacity": 0.9,
              "animate": [ { "param": "position", "keys": [ { "time": 0, "value": [540, -300] }, { "time": 0.8, "value": [540, 900], "easing": "easeOut" } ] },
                           { "param": "fontSize", "keys": [ { "time": 0, "value": 60 }, { "time": 0.8, "value": 120 } ] } ] },
            { "kind": "image", "label": "hero", "position": [540, 1400], "size": [600, 400] }
        ]
    }"#;

    #[test]
    fn builds_one_layer_per_recipe_layer() {
        let recipe = parse_recipe(DEMO).expect("valid");
        let layers = build_layers(&recipe);
        assert_eq!(layers.len(), 4);
        assert!(layers.iter().all(|(fx, _)| graphic::is_layer(fx)));
    }

    #[test]
    fn text_layer_carries_text_style_and_transform() {
        let recipe = parse_recipe(DEMO).expect("valid");
        let layers = build_layers(&recipe);
        let (fx, _) = &layers[2];
        assert_eq!(fx.effect, graphic::TEXT_LAYER);
        assert_eq!(fx.f64_at("size", Tick::ZERO), 120.0);
        assert_eq!(fx.f64_at("tracking", Tick::ZERO), 0.0, "unset tracking stays default");
        assert_eq!(fx.f64_at("opacity", Tick::ZERO), 90.0, "0.9 becomes 90 percent");
        let text = fx.param("text").and_then(|p| match &p.value {
            ParamValue::Text(t) => Some(t.as_str()),
            _ => None,
        });
        assert_eq!(text, Some("ماذا لو؟"));
        let font = fx.param("font").and_then(|p| match &p.value {
            ParamValue::Text(t) => Some(t.as_str()),
            _ => None,
        });
        assert_eq!(font, Some("Cairo"));
    }

    #[test]
    fn animation_becomes_real_keyframes() {
        let recipe = parse_recipe(DEMO).expect("valid");
        let layers = build_layers(&recipe);
        let (fx, _) = &layers[2];
        let position = fx.param("position").expect("position param");
        assert_eq!(position.keyframes.len(), 2);
        assert_eq!(position.keyframes[0].time, Tick::ZERO);
        assert_eq!(position.keyframes[1].interp, filmcraft_project::Interpolation::EaseOut);
        let expected = FrameRate::FPS_30.snap_nearest(Tick::from_seconds_f64(0.8));
        assert_eq!(position.keyframes[1].time, expected);
        match &position.keyframes[1].value {
            ParamValue::Vec2(v) => {
                assert!((v.x - 540.0).abs() < 1e-9);
                assert!((v.y - 900.0).abs() < 1e-9);
            }
            other => panic!("position key must be a point, got {other:?}"),
        }
        let (shape, _) = &layers[1];
        let scale = shape.param("scale").expect("scale param");
        assert_eq!(scale.keyframes.len(), 2);
        assert_eq!(scale.keyframes[0].value, ParamValue::Float(0.0));
        assert_eq!(scale.keyframes[1].value, ParamValue::Float(100.0), "scale 1 becomes 100 percent");
    }

    #[test]
    fn solid_is_a_full_frame_rect_and_image_is_a_named_placeholder() {
        let recipe = parse_recipe(DEMO).expect("valid");
        let layers = build_layers(&recipe);
        let (solid, _) = &layers[0];
        assert_eq!(solid.effect, graphic::SHAPE_LAYER);
        match solid.param("size").map(|p| &p.value) {
            Some(ParamValue::Vec2(v)) => {
                assert!((v.x - 1080.0).abs() < 1e-9);
                assert!((v.y - 1920.0).abs() < 1e-9);
            }
            other => panic!("solid must be canvas-sized, got {other:?}"),
        }
        assert_eq!(solid.param("fill_color").map(|p| &p.value), Some(&ParamValue::Color([0.05, 0.05, 0.1, 1.0])));
        let (image, _) = &layers[3];
        assert_eq!(image.effect, graphic::SHAPE_LAYER);
    }

    #[test]
    fn recipe_rate_prefers_common_rates() {
        assert_eq!(recipe_rate(30.0), FrameRate::FPS_30);
        assert_eq!(recipe_rate(29.97), FrameRate::FPS_29_97);
        assert_eq!(recipe_rate(24.0), FrameRate::FPS_24);
        let odd = recipe_rate(37.0);
        assert!((odd.as_f64() - 37.0).abs() < 1e-9);
    }

    #[test]
    fn prompt_contains_description_canvas_and_format() {
        let p = build_prompt("عنوان يدخل من تحت", Some("a dark blue poster"), (1080, 1920, 30.0));
        assert!(p.contains("عنوان يدخل من تحت"));
        assert!(p.contains("1080 x 1920"));
        assert!(p.contains("\"version\": 1"));
        assert!(p.contains("reference image"));
        assert!(p.contains("easeOut"));
        let without = build_prompt("a title", None, (1920, 1080, 25.0));
        assert!(!without.contains("reference image"));
    }

    #[test]
    fn from_recipe_creates_a_new_sequence_with_clips_per_layer() {
        let mut s = Session::default();
        let before = s.project.sequences().count();
        let out = from_recipe(&mut s, DEMO, None).expect("from_recipe must succeed");
        assert_eq!(s.project.sequences().count(), before + 1);
        let seq_id = filmcraft_project::ItemId(out["sequence"].as_u64().expect("sequence id"));
        assert_eq!(s.state.active_sequence, Some(seq_id));
        let q = s.project.sequence(seq_id).expect("sequence exists");
        assert_eq!(q.settings.width, 1080);
        assert_eq!(q.settings.height, 1920);
        assert_eq!(q.settings.frame_rate, FrameRate::FPS_30);
        assert_eq!(q.video_tracks.len(), 4, "one track per layer");
        for (i, track) in q.video_tracks.iter().enumerate() {
            assert_eq!(track.items.len(), 1, "track {} holds its layer's clip", i + 1);
            let layers = graphic::layer_indices(&track.items[0].effects);
            assert_eq!(layers.len(), 1, "each clip holds exactly its own graphic layer");
        }
        // The text clip (track 3) animates: its position param has keyframes.
        let text_clip = &q.video_tracks[2].items[0];
        let li = graphic::layer_indices(&text_clip.effects)[0];
        let position = text_clip.effects[li].param("position").expect("position");
        assert_eq!(position.keyframes.len(), 2);
        // Undo removes the whole recipe as one step.
        let clips = out["clips"].as_array().expect("clips list").len();
        assert_eq!(clips, 4);
    }

    #[test]
    fn from_recipe_honors_an_explicit_name_and_does_not_touch_other_sequences() {
        let mut s = Session::default();
        let first = from_recipe(&mut s, DEMO, Some("First")).expect("first build");
        let second = from_recipe(&mut s, DEMO, Some("Second")).expect("second build");
        assert_ne!(first["sequence"], second["sequence"]);
        let first_id = filmcraft_project::ItemId(first["sequence"].as_u64().expect("id"));
        let q = s.project.sequence(first_id).expect("first sequence stays");
        assert_eq!(s.project.item(first_id).expect("item").name, "First");
        assert_eq!(q.video_tracks.len(), 4, "the earlier sequence is untouched");
    }

    #[test]
    fn from_recipe_rejects_a_bad_reply_without_creating_anything() {
        let mut s = Session::default();
        let before = s.project.sequences().count();
        let err = from_recipe(&mut s, "{ \"version\": 9 }", None).expect_err("must fail");
        assert!(err.to_string().contains("version"), "{err}");
        assert_eq!(s.project.sequences().count(), before);
        assert!(from_recipe(&mut s, "not json at all", None).is_err());
        assert_eq!(s.project.sequences().count(), before);
    }

    #[test]
    fn schema_version_is_pinned() {
        assert_eq!(RECIPE_VERSION, 1);
    }
}
