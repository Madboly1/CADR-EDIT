//! Motion recipes: the declarative, data-only format behind the Gemini-assisted motion tool.
//!
//! The user describes a scene in words (a reference image stays on his machine), Kadr prepares a
//! prompt for an external Gemini conversation, and Gemini answers with one JSON document in the
//! shape below. The reply is *data, never code*: no expressions, no URLs, no file paths, no
//! executable content of any kind. [`parse_recipe`] validates every value against fixed bounds
//! before anything touches a project, collects every problem it finds, and rejects the whole
//! recipe - an invalid recipe is never partially applied.
//!
//! ## Format (version 1)
//!
//! ```json
//! {
//!   "version": 1,
//!   "name": "Title pop",
//!   "canvas": { "width": 1080, "height": 1920, "fps": 30, "duration": 5.0 },
//!   "layers": [
//!     {
//!       "kind": "text",
//!       "name": "headline",
//!       "text": "ماذا لو؟",
//!       "font_size": 110,
//!       "color": [1, 1, 1, 1],
//!       "position": [540, 900],
//!       "animate": [
//!         {
//!           "param": "position",
//!           "keys": [
//!             { "time": 0.0, "value": [540, -200], "easing": "easeOut" },
//!             { "time": 0.7, "value": [540, 900] }
//!           ]
//!         }
//!       ]
//!     },
//!     { "kind": "solid", "fill": [0, 0, 0, 1] }
//!   ]
//! }
//! ```
//!
//! - `canvas`: output size in pixels, frame rate, total duration in seconds. Required.
//! - `layers`: back to front, 1..=32 layers. `kind` is `text`, `shape`, `solid` or `image`.
//! - Layer `start` / `duration` are seconds on the canvas timeline; both optional (default: the
//!   whole canvas duration). Keyframe `time` values are seconds relative to the layer's own start.
//! - Static `position` / `scale` / `rotation` / `opacity` set the base transform of any layer
//!   (except `solid`, which fills the frame). Defaults: canvas center, 1, 0, 1.
//! - `animate` entries name one whitelisted parameter each and hold 1..=64 keys with strictly
//!   increasing times. Allowed per kind:
//!   `text`: position, scale, rotation, opacity, blur, fontSize, tracking, fill;
//!   `shape`: position, scale, rotation, opacity, blur, fill;
//!   `image`: position, scale, rotation, opacity, blur;
//!   `solid`: opacity, fill.
//! - `easing` is one of `linear` (default), `hold`, `easeIn`, `easeOut`, `bezier`, `autoBezier`,
//!   `continuousBezier` - the interpolation of the segment *leaving* that key.

use filmcraft_project::Interpolation;
use serde::Deserialize;

/// Raw recipe JSON larger than this is rejected before parsing, so a hostile or runaway reply
/// cannot make the editor allocate unbounded memory.
pub const MAX_RECIPE_BYTES: usize = 1024 * 1024;
/// The only recipe version this build understands.
pub const RECIPE_VERSION: u32 = 1;

const MAX_LAYERS: usize = 32;
const MAX_TRACKS_PER_LAYER: usize = 8;
const MAX_KEYS_PER_TRACK: usize = 64;
const MAX_NAME_CHARS: usize = 120;
const MAX_TEXT_CHARS: usize = 500;
const MAX_DIMENSION: u32 = 7680;
const MIN_DIMENSION: u32 = 16;
const MAX_CANVAS_SECONDS: f64 = 600.0;
const MAX_SCALE: f64 = 10.0;
const MAX_ROTATION_DEG: f64 = 3600.0;
const MAX_BLUR: f64 = 250.0;
const MAX_FONT_SIZE: f64 = 500.0;
const MIN_FONT_SIZE: f64 = 8.0;
const MAX_TRACKING: f64 = 1000.0;
const MIN_TRACKING: f64 = -500.0;
/// Keyframe times may touch the layer end; this absorbs float round-off in model output.
const TIME_EPSILON: f64 = 1e-6;

/// One thing wrong with a recipe, pointed at its place in the JSON (`layers[2].animate[0]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeIssue {
    pub path: String,
    pub message: String,
}

impl RecipeIssue {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { path: path.into(), message: message.into() }
    }
}

/// Why a Gemini reply was not accepted as a motion recipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipeReject {
    /// Larger than [`MAX_RECIPE_BYTES`]; not parsed at all.
    TooLarge(usize),
    /// Not JSON, or JSON that does not match the recipe structure.
    Syntax(String),
    /// Structurally JSON of the right shape, but one or more values break the rules.
    Invalid(Vec<RecipeIssue>),
}

impl std::fmt::Display for RecipeReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecipeReject::TooLarge(n) => write!(f, "the reply is {n} bytes; a motion recipe is at most {} bytes", MAX_RECIPE_BYTES),
            RecipeReject::Syntax(msg) => write!(f, "the reply is not a motion recipe: {msg}"),
            RecipeReject::Invalid(issues) => {
                write!(f, "the motion recipe has {} problem(s): ", issues.len())?;
                for (i, issue) in issues.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{}: {}", issue.path, issue.message)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RecipeReject {}

/// Output rectangle, frame rate and total length of the animation.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCanvas {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Total seconds.
    pub duration: f64,
}

/// The layer kinds a recipe may contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayerKind {
    Text,
    Shape,
    Solid,
    Image,
}

impl LayerKind {
    fn label(self) -> &'static str {
        match self {
            LayerKind::Text => "text",
            LayerKind::Shape => "shape",
            LayerKind::Solid => "solid",
            LayerKind::Image => "image",
        }
    }
}

/// Which shape a `shape` layer draws.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShapeKind {
    #[default]
    Rect,
    Ellipse,
}

/// The interpolation of the segment leaving a keyframe; maps onto the project's keyframe engine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Easing {
    #[default]
    Linear,
    Hold,
    EaseIn,
    EaseOut,
    Bezier,
    AutoBezier,
    ContinuousBezier,
}

impl Easing {
    /// The equivalent project interpolation.
    pub fn to_interpolation(self) -> Interpolation {
        match self {
            Easing::Linear => Interpolation::Linear,
            Easing::Hold => Interpolation::Hold,
            Easing::EaseIn => Interpolation::EaseIn,
            Easing::EaseOut => Interpolation::EaseOut,
            Easing::Bezier => Interpolation::Bezier,
            Easing::AutoBezier => Interpolation::AutoBezier,
            Easing::ContinuousBezier => Interpolation::ContinuousBezier,
        }
    }
}

/// A keyframe value: a single number, an `[x, y]` pair, or an `[r, g, b, a]` color.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum KeyValue {
    Number(f64),
    Vec2([f64; 2]),
    Color([f64; 4]),
}

/// One keyframe of an animated parameter; `time` is seconds relative to the layer start.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeKey {
    pub time: f64,
    pub value: KeyValue,
    #[serde(default)]
    pub easing: Easing,
}

/// The parameters a recipe may animate. `Fill` is the layer's color (text color, shape fill or
/// solid color).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
pub enum AnimParam {
    #[serde(rename = "position")]
    Position,
    #[serde(rename = "scale")]
    Scale,
    #[serde(rename = "rotation")]
    Rotation,
    #[serde(rename = "opacity")]
    Opacity,
    #[serde(rename = "blur")]
    Blur,
    #[serde(rename = "fontSize")]
    FontSize,
    #[serde(rename = "tracking")]
    Tracking,
    #[serde(rename = "fill")]
    Fill,
}

impl AnimParam {
    fn label(self) -> &'static str {
        match self {
            AnimParam::Position => "position",
            AnimParam::Scale => "scale",
            AnimParam::Rotation => "rotation",
            AnimParam::Opacity => "opacity",
            AnimParam::Blur => "blur",
            AnimParam::FontSize => "fontSize",
            AnimParam::Tracking => "tracking",
            AnimParam::Fill => "fill",
        }
    }

    /// Whether this parameter may be animated on a layer of the given kind.
    fn allowed_on(self, kind: LayerKind) -> bool {
        match kind {
            LayerKind::Text => true,
            LayerKind::Shape => !matches!(self, AnimParam::FontSize | AnimParam::Tracking),
            LayerKind::Image => matches!(self, AnimParam::Position | AnimParam::Scale | AnimParam::Rotation | AnimParam::Opacity | AnimParam::Blur),
            LayerKind::Solid => matches!(self, AnimParam::Opacity | AnimParam::Fill),
        }
    }
}

/// One animated parameter of one layer.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamTrack {
    pub param: AnimParam,
    pub keys: Vec<RecipeKey>,
}

/// One layer of the scene, back to front.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeLayer {
    pub kind: LayerKind,
    /// Optional display name; defaults to `kind N` when the clip is built.
    pub name: Option<String>,
    /// Seconds on the canvas timeline; default 0.
    #[serde(default)]
    pub start: f64,
    /// Seconds; default is the rest of the canvas after `start`.
    pub duration: Option<f64>,
    /// Text layers: the string to draw (required for `text`).
    pub text: Option<String>,
    /// Text layers: font family name; default is the project font.
    pub font: Option<String>,
    /// Text layers: base font size in pixels; default 96.
    pub font_size: Option<f64>,
    /// Text layers: base color; default white.
    pub color: Option<[f64; 4]>,
    /// Text layers: extra letter spacing in thousandths of an em; default 0.
    pub tracking: Option<f64>,
    /// Shape layers: which shape; default `rect`.
    pub shape: Option<ShapeKind>,
    /// Shape and solid layers: fill color; defaults white (shape) / black (solid).
    pub fill: Option<[f64; 4]>,
    /// Shape layers: size in pixels (required for `shape`). Image layers: display size; default
    /// is the whole canvas.
    pub size: Option<[f64; 2]>,
    /// Image layers: a short label for the slot the user fills on his own machine (required for
    /// `image`). Never a path or URL.
    pub label: Option<String>,
    /// Base transform of the layer; defaults are canvas center, 1, 0, 1.
    pub position: Option<[f64; 2]>,
    pub scale: Option<f64>,
    pub rotation: Option<f64>,
    pub opacity: Option<f64>,
    /// Animated parameters; at most [`MAX_TRACKS_PER_LAYER`].
    pub animate: Option<Vec<ParamTrack>>,
}

impl RecipeLayer {
    /// The layer's length in seconds once defaults are applied.
    pub fn resolved_duration(&self, canvas: &RecipeCanvas) -> f64 {
        self.duration.unwrap_or((canvas.duration - self.start).max(0.0))
    }
}

/// A validated motion recipe: one canvas plus its layers.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MotionRecipe {
    pub version: u32,
    pub name: Option<String>,
    pub canvas: RecipeCanvas,
    pub layers: Vec<RecipeLayer>,
}

/// Strip an optional UTF-8 BOM and Markdown ```json ... ``` fences from a model reply.
fn strip_code_fences(raw: &str) -> &str {
    let mut s = raw.trim().trim_start_matches('\u{FEFF}').trim();
    if let Some(rest) = s.strip_prefix("```") {
        // Drop an optional language tag (e.g. `json`) up to the first newline.
        s = rest.find('\n').map_or(rest, |nl| &rest[nl + 1..]);
        if let Some((before, _)) = s.rsplit_once("```") {
            s = before;
        }
    }
    s.trim()
}

/// Parse and validate a Gemini reply into a [`MotionRecipe`].
///
/// Anything wrong rejects the whole recipe with every problem found; nothing is clamped,
/// guessed or partially applied. Input is treated as hostile: size-capped before parsing, every
/// number checked, no panics.
pub fn parse_recipe(raw: &str) -> Result<MotionRecipe, RecipeReject> {
    if raw.len() > MAX_RECIPE_BYTES {
        return Err(RecipeReject::TooLarge(raw.len()));
    }
    let clean = strip_code_fences(raw);
    let recipe: MotionRecipe = serde_json::from_str(clean).map_err(|e| RecipeReject::Syntax(e.to_string()))?;
    let mut issues = Vec::new();
    validate_recipe(&recipe, &mut issues);
    if issues.is_empty() { Ok(recipe) } else { Err(RecipeReject::Invalid(issues)) }
}

fn validate_recipe(recipe: &MotionRecipe, issues: &mut Vec<RecipeIssue>) {
    if recipe.version != RECIPE_VERSION {
        issues.push(RecipeIssue::new("version", format!("must be {RECIPE_VERSION}; this build does not understand version {}", recipe.version)));
    }
    if let Some(name) = &recipe.name {
        check_chars("name", name, MAX_NAME_CHARS, issues);
    }
    validate_canvas(&recipe.canvas, issues);
    if recipe.layers.is_empty() {
        issues.push(RecipeIssue::new("layers", "needs at least one layer".to_string()));
    }
    if recipe.layers.len() > MAX_LAYERS {
        issues.push(RecipeIssue::new("layers", format!("has {} layers; at most {MAX_LAYERS} are allowed", recipe.layers.len())));
    }
    for (i, layer) in recipe.layers.iter().enumerate() {
        validate_layer(&recipe.canvas, i, layer, issues);
    }
}

fn validate_canvas(canvas: &RecipeCanvas, issues: &mut Vec<RecipeIssue>) {
    for (field, value) in [("canvas.width", canvas.width), ("canvas.height", canvas.height)] {
        if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&value) {
            issues.push(RecipeIssue::new(field, format!("{value} px is outside {MIN_DIMENSION}..={MAX_DIMENSION}")));
        }
    }
    check_number("canvas.fps", canvas.fps, 1.0, 120.0, issues);
    check_number("canvas.duration", canvas.duration, 0.1, MAX_CANVAS_SECONDS, issues);
}

fn validate_layer(canvas: &RecipeCanvas, index: usize, layer: &RecipeLayer, issues: &mut Vec<RecipeIssue>) {
    let path = format!("layers[{index}]");
    check_number(&format!("{path}.start"), layer.start, 0.0, MAX_CANVAS_SECONDS, issues);
    let duration = layer.resolved_duration(canvas);
    if let Some(declared) = layer.duration {
        check_number(&format!("{path}.duration"), declared, 0.01, MAX_CANVAS_SECONDS, issues);
    }
    if duration <= 0.0 {
        issues.push(RecipeIssue::new(&path, format!("starts at {} s but the canvas ends at {} s", layer.start, canvas.duration)));
    } else if layer.start + duration > canvas.duration + TIME_EPSILON {
        issues.push(RecipeIssue::new(&path, format!("ends at {:.3} s, past the canvas duration of {} s", layer.start + duration, canvas.duration)));
    }
    if let Some(name) = &layer.name {
        check_chars(&format!("{path}.name"), name, MAX_NAME_CHARS, issues);
    }
    validate_layer_transform(&path, layer, issues);
    validate_layer_kind_fields(&path, layer, issues);
    if let Some(tracks) = &layer.animate {
        validate_tracks(&path, layer, duration, tracks, issues);
    }
}

fn validate_layer_transform(path: &str, layer: &RecipeLayer, issues: &mut Vec<RecipeIssue>) {
    if let Some(position) = layer.position {
        check_vec2(&format!("{path}.position"), position, issues);
    }
    if let Some(scale) = layer.scale {
        check_number(&format!("{path}.scale"), scale, 0.0, MAX_SCALE, issues);
    }
    if let Some(rotation) = layer.rotation {
        check_number(&format!("{path}.rotation"), rotation, -MAX_ROTATION_DEG, MAX_ROTATION_DEG, issues);
    }
    if let Some(opacity) = layer.opacity {
        check_number(&format!("{path}.opacity"), opacity, 0.0, 1.0, issues);
    }
    if layer.kind == LayerKind::Solid && (layer.position.is_some() || layer.scale.is_some() || layer.rotation.is_some()) {
        issues.push(RecipeIssue::new(path, "a solid fills the whole frame; only opacity and fill may move".to_string()));
    }
}

fn validate_layer_kind_fields(path: &str, layer: &RecipeLayer, issues: &mut Vec<RecipeIssue>) {
    match layer.kind {
        LayerKind::Text => validate_text_fields(path, layer, issues),
        LayerKind::Shape => validate_shape_fields(path, layer, issues),
        LayerKind::Solid => {
            if let Some(fill) = layer.fill {
                check_color(&format!("{path}.fill"), fill, issues);
            }
        }
        LayerKind::Image => validate_image_fields(path, layer, issues),
    }
    // Fields that only make sense on other kinds are rejected no matter the kind.
    if layer.kind != LayerKind::Text {
        for present in [("text", layer.text.is_some()), ("font", layer.font.is_some()), ("font_size", layer.font_size.is_some()), ("color", layer.color.is_some()), ("tracking", layer.tracking.is_some())] {
            if present.1 {
                issues.push(RecipeIssue::new(path, format!("`{}` only applies to text layers", present.0)));
            }
        }
    }
    if layer.kind != LayerKind::Shape && layer.shape.is_some() {
        issues.push(RecipeIssue::new(path, "`shape` only applies to shape layers".to_string()));
    }
    if layer.kind != LayerKind::Shape && layer.kind != LayerKind::Solid && layer.fill.is_some() {
        issues.push(RecipeIssue::new(path, "`fill` only applies to shape and solid layers".to_string()));
    }
    if layer.kind != LayerKind::Image && layer.label.is_some() {
        issues.push(RecipeIssue::new(path, "`label` only applies to image layers".to_string()));
    }
    if layer.kind != LayerKind::Shape && layer.kind != LayerKind::Image && layer.size.is_some() {
        issues.push(RecipeIssue::new(path, "`size` only applies to shape and image layers".to_string()));
    }
}

fn validate_text_fields(path: &str, layer: &RecipeLayer, issues: &mut Vec<RecipeIssue>) {
    match &layer.text {
        None => issues.push(RecipeIssue::new(path, "a text layer needs its `text`".to_string())),
        Some(text) => {
            if text.trim().is_empty() {
                issues.push(RecipeIssue::new(path, "a text layer needs non-empty `text`".to_string()));
            } else {
                check_chars(&format!("{path}.text"), text, MAX_TEXT_CHARS, issues);
            }
        }
    }
    if let Some(font) = &layer.font {
        check_chars(&format!("{path}.font"), font, MAX_NAME_CHARS, issues);
    }
    if let Some(size) = layer.font_size {
        check_number(&format!("{path}.font_size"), size, MIN_FONT_SIZE, MAX_FONT_SIZE, issues);
    }
    if let Some(color) = layer.color {
        check_color(&format!("{path}.color"), color, issues);
    }
    if let Some(tracking) = layer.tracking {
        check_number(&format!("{path}.tracking"), tracking, MIN_TRACKING, MAX_TRACKING, issues);
    }
}

fn validate_shape_fields(path: &str, layer: &RecipeLayer, issues: &mut Vec<RecipeIssue>) {
    match layer.size {
        None => issues.push(RecipeIssue::new(path, "a shape layer needs its `size` in pixels".to_string())),
        Some(size) => {
            for (axis, value) in [("width", size[0]), ("height", size[1])] {
                check_number(&format!("{path}.size.{axis}"), value, 1.0, MAX_DIMENSION as f64, issues);
            }
        }
    }
    if let Some(fill) = layer.fill {
        check_color(&format!("{path}.fill"), fill, issues);
    }
}

fn validate_image_fields(path: &str, layer: &RecipeLayer, issues: &mut Vec<RecipeIssue>) {
    match &layer.label {
        None => issues.push(RecipeIssue::new(path, "an image layer needs a short `label` for the slot the user fills".to_string())),
        Some(label) => check_chars(&format!("{path}.label"), label, MAX_NAME_CHARS, issues),
    }
    if let Some(size) = layer.size {
        for (axis, value) in [("width", size[0]), ("height", size[1])] {
            check_number(&format!("{path}.size.{axis}"), value, 1.0, MAX_DIMENSION as f64, issues);
        }
    }
}

fn validate_tracks(path: &str, layer: &RecipeLayer, duration: f64, tracks: &[ParamTrack], issues: &mut Vec<RecipeIssue>) {
    if tracks.len() > MAX_TRACKS_PER_LAYER {
        issues.push(RecipeIssue::new(path, format!("animates {} parameters; at most {MAX_TRACKS_PER_LAYER} are allowed", tracks.len())));
    }
    let mut seen = Vec::with_capacity(tracks.len());
    for (t, track) in tracks.iter().enumerate() {
        let tpath = format!("{path}.animate[{t}]");
        if seen.contains(&track.param) {
            issues.push(RecipeIssue::new(&tpath, format!("`{}` is animated twice on this layer", track.param.label())));
        }
        seen.push(track.param);
        if !track.param.allowed_on(layer.kind) {
            issues.push(RecipeIssue::new(&tpath, format!("`{}` cannot animate a {} layer", track.param.label(), layer.kind.label())));
        }
        validate_track_keys(&tpath, track, duration, issues);
    }
}

fn validate_track_keys(path: &str, track: &ParamTrack, duration: f64, issues: &mut Vec<RecipeIssue>) {
    if track.keys.is_empty() {
        issues.push(RecipeIssue::new(path, "needs at least one keyframe".to_string()));
    }
    if track.keys.len() > MAX_KEYS_PER_TRACK {
        issues.push(RecipeIssue::new(path, format!("has {} keyframes; at most {MAX_KEYS_PER_TRACK} are allowed", track.keys.len())));
    }
    let mut previous: Option<f64> = None;
    for (k, key) in track.keys.iter().enumerate() {
        let kpath = format!("{path}.keys[{k}]");
        check_time(&kpath, key.time, duration, previous, issues);
        previous = Some(key.time);
        check_key_value(&kpath, track.param, key.value, issues);
    }
}

fn check_time(path: &str, time: f64, duration: f64, previous: Option<f64>, issues: &mut Vec<RecipeIssue>) {
    if !time.is_finite() || time < 0.0 || time > duration + TIME_EPSILON {
        issues.push(RecipeIssue::new(path, format!("time {time} is outside the layer's 0..={duration:.3} seconds")));
        return;
    }
    if let Some(prev) = previous {
        if time <= prev {
            issues.push(RecipeIssue::new(path, format!("time {time} must come after the previous key's {prev}")));
        }
    }
}

fn check_key_value(path: &str, param: AnimParam, value: KeyValue, issues: &mut Vec<RecipeIssue>) {
    match (param, value) {
        (AnimParam::Position, KeyValue::Vec2(v)) => check_vec2(&format!("{path}.value"), v, issues),
        (AnimParam::Fill, KeyValue::Color(c)) => check_color(&format!("{path}.value"), c, issues),
        (AnimParam::Scale, KeyValue::Number(n)) => check_number(&format!("{path}.value"), n, 0.0, MAX_SCALE, issues),
        (AnimParam::Rotation, KeyValue::Number(n)) => check_number(&format!("{path}.value"), n, -MAX_ROTATION_DEG, MAX_ROTATION_DEG, issues),
        (AnimParam::Opacity, KeyValue::Number(n)) => check_number(&format!("{path}.value"), n, 0.0, 1.0, issues),
        (AnimParam::Blur, KeyValue::Number(n)) => check_number(&format!("{path}.value"), n, 0.0, MAX_BLUR, issues),
        (AnimParam::FontSize, KeyValue::Number(n)) => check_number(&format!("{path}.value"), n, MIN_FONT_SIZE, MAX_FONT_SIZE, issues),
        (AnimParam::Tracking, KeyValue::Number(n)) => check_number(&format!("{path}.value"), n, MIN_TRACKING, MAX_TRACKING, issues),
        (AnimParam::Position, _) => issues.push(RecipeIssue::new(path, "`position` values are [x, y] pairs".to_string())),
        (AnimParam::Fill, _) => issues.push(RecipeIssue::new(path, "`fill` values are [r, g, b, a] colors".to_string())),
        _ => issues.push(RecipeIssue::new(path, format!("`{}` values are single numbers", param.label()))),
    }
}

fn check_number(path: &str, value: f64, min: f64, max: f64, issues: &mut Vec<RecipeIssue>) {
    if !value.is_finite() || value < min || value > max {
        issues.push(RecipeIssue::new(path, format!("{value} is outside {min}..={max}")));
    }
}

fn check_vec2(path: &str, value: [f64; 2], issues: &mut Vec<RecipeIssue>) {
    for component in value {
        // Positions may sit off-frame for slide-ins; only the absurd is rejected.
        check_number(path, component, -4.0 * MAX_DIMENSION as f64, 4.0 * MAX_DIMENSION as f64, issues);
    }
}

fn check_color(path: &str, value: [f64; 4], issues: &mut Vec<RecipeIssue>) {
    for component in value {
        check_number(path, component, 0.0, 1.0, issues);
    }
}

fn check_chars(path: &str, value: &str, max: usize, issues: &mut Vec<RecipeIssue>) {
    if value.chars().count() > max {
        issues.push(RecipeIssue::new(path, format!("{} characters is over the limit of {max}", value.chars().count())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{
        "version": 1,
        "canvas": { "width": 1080, "height": 1920, "fps": 30, "duration": 5.0 },
        "layers": [ { "kind": "text", "text": "Hello" } ]
    }"#;

    fn good_layer_json() -> &'static str {
        r#"{
            "kind": "text",
            "name": "headline",
            "text": "ماذا لو؟",
            "font": "Cairo",
            "font_size": 110,
            "color": [1, 1, 1, 0.95],
            "tracking": 20,
            "position": [540, 900],
            "scale": 1,
            "rotation": 0,
            "opacity": 1,
            "animate": [
                { "param": "position", "keys": [
                    { "time": 0.0, "value": [540, -200], "easing": "easeOut" },
                    { "time": 0.7, "value": [540, 900] }
                ] },
                { "param": "opacity", "keys": [
                    { "time": 0.0, "value": 0 },
                    { "time": 0.5, "value": 1, "easing": "linear" }
                ] }
            ]
        }"#
    }

    fn recipe_with_layer(layer: &str) -> String {
        format!(
            r#"{{ "version": 1, "name": "test", "canvas": {{ "width": 1080, "height": 1920, "fps": 30, "duration": 5.0 }}, "layers": [ {layer}, {{ "kind": "solid", "fill": [0, 0, 0, 1] }} ] }}"#
        )
    }

    fn issues_of(json: &str) -> Vec<RecipeIssue> {
        match parse_recipe(json) {
            Err(RecipeReject::Invalid(issues)) => issues,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn minimal_recipe_parses() {
        let recipe = parse_recipe(MINIMAL).expect("minimal recipe must parse");
        assert_eq!(recipe.version, 1);
        assert_eq!(recipe.layers.len(), 1);
        assert_eq!(recipe.layers[0].start, 0.0);
        assert_eq!(recipe.layers[0].resolved_duration(&recipe.canvas), 5.0);
    }

    #[test]
    fn full_layer_with_tracks_parses() {
        let json = recipe_with_layer(good_layer_json());
        let recipe = parse_recipe(&json).expect("full recipe must parse: {}", json);
        let tracks = recipe.layers[0].animate.as_ref().expect("tracks");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].keys[0].easing, Easing::EaseOut);
        assert_eq!(tracks[0].keys[0].easing.to_interpolation(), Interpolation::EaseOut);
        assert_eq!(tracks[1].keys[0].easing, Easing::Linear, "missing easing defaults to linear");
    }

    #[test]
    fn markdown_fences_and_bom_are_stripped() {
        let fenced = format!("\u{FEFF}```json\n{MINIMAL}\n```\n");
        parse_recipe(&fenced).expect("fenced reply must parse");
    }

    #[test]
    fn syntax_error_is_reported_not_panicked() {
        match parse_recipe("{ not json") {
            Err(RecipeReject::Syntax(_)) => {}
            other => panic!("expected Syntax, got {other:?}"),
        }
    }

    #[test]
    fn oversized_reply_is_rejected_before_parsing() {
        let big = " ".repeat(MAX_RECIPE_BYTES + 1);
        match parse_recipe(&big) {
            Err(RecipeReject::TooLarge(_)) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn truncated_and_garbled_input_never_panics() {
        let good = recipe_with_layer(good_layer_json());
        // Truncations at every byte boundary of a valid recipe.
        for end in 0..good.len() {
            let _ = parse_recipe(&good[..end]);
        }
        // Bit flips across the whole document.
        let bytes = good.as_bytes();
        for i in 0..bytes.len() {
            let mut mutated = bytes.to_vec();
            mutated[i] ^= 0x5A;
            if let Ok(text) = String::from_utf8(mutated) {
                let _ = parse_recipe(&text);
            }
        }
        // Deeply nested arrays stay inside serde_json's recursion limit.
        let deep = "[".repeat(10_000);
        let _ = parse_recipe(&deep);
    }

    #[test]
    fn wrong_version_is_rejected() {
        let json = MINIMAL.replace("\"version\": 1", "\"version\": 2");
        let issues = issues_of(&json);
        assert!(issues.iter().any(|i| i.path == "version"), "{issues:?}");
    }

    #[test]
    fn canvas_bounds_are_enforced() {
        let json = MINIMAL.replace("\"width\": 1080", "\"width\": 0");
        assert!(issues_of(&json).iter().any(|i| i.path == "canvas.width"));
        let json = MINIMAL.replace("\"height\": 1920", "\"height\": 8000");
        assert!(issues_of(&json).iter().any(|i| i.path == "canvas.height"));
        let json = MINIMAL.replace("\"fps\": 30", "\"fps\": 0.5");
        assert!(issues_of(&json).iter().any(|i| i.path == "canvas.fps"));
        let json = MINIMAL.replace("\"duration\": 5.0", "\"duration\": 9999");
        assert!(issues_of(&json).iter().any(|i| i.path == "canvas.duration"));
    }

    #[test]
    fn empty_layers_are_rejected() {
        let json = MINIMAL.replace(r#"[ { "kind": "text", "text": "Hello" } ]"#, "[]");
        assert!(issues_of(&json).iter().any(|i| i.path == "layers"));
    }

    #[test]
    fn text_layer_needs_text() {
        let json = recipe_with_layer(r#"{ "kind": "text" }"#);
        assert!(issues_of(&json).iter().any(|i| i.message.contains("needs its `text`")));
    }

    #[test]
    fn long_text_is_rejected() {
        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        let json = recipe_with_layer(&format!(r#"{{ "kind": "text", "text": "{long}" }}"#));
        assert!(issues_of(&json).iter().any(|i| i.path == "layers[0].text"));
    }

    #[test]
    fn misplaced_kind_fields_are_rejected() {
        let json = recipe_with_layer(r#"{ "kind": "image", "label": "ref", "text": "nope", "fill": [1, 1, 1, 1] }"#);
        let issues = issues_of(&json);
        assert!(issues.iter().any(|i| i.message.contains("`text` only applies to text layers")), "{issues:?}");
        assert!(issues.iter().any(|i| i.message.contains("`fill` only applies")), "{issues:?}");
    }

    #[test]
    fn image_layer_needs_label_and_shape_needs_size() {
        let json = recipe_with_layer(r#"{ "kind": "image" }"#);
        assert!(issues_of(&json).iter().any(|i| i.message.contains("needs a short `label`")));
        let json = recipe_with_layer(r#"{ "kind": "shape" }"#);
        assert!(issues_of(&json).iter().any(|i| i.message.contains("needs its `size`")));
    }

    #[test]
    fn layer_must_fit_the_canvas() {
        let json = recipe_with_layer(r#"{ "kind": "text", "text": "Hi", "start": 4.0, "duration": 2.0 }"#);
        assert!(issues_of(&json).iter().any(|i| i.message.contains("past the canvas duration")));
        let json = recipe_with_layer(r#"{ "kind": "text", "text": "Hi", "start": 6.0 }"#);
        assert!(issues_of(&json).iter().any(|i| i.message.contains("the canvas ends at")));
    }

    #[test]
    fn duplicate_and_forbidden_params_are_rejected() {
        let dup = r#"{ "kind": "shape", "size": [100, 100], "animate": [
            { "param": "opacity", "keys": [ { "time": 0, "value": 0 } ] },
            { "param": "opacity", "keys": [ { "time": 0, "value": 1 } ] },
            { "param": "fontSize", "keys": [ { "time": 0, "value": 100 } ] }
        ] }"#;
        let issues = issues_of(&recipe_with_layer(dup));
        assert!(issues.iter().any(|i| i.message.contains("animated twice")), "{issues:?}");
        assert!(issues.iter().any(|i| i.message.contains("cannot animate a shape layer")), "{issues:?}");
    }

    #[test]
    fn keyframe_rules_are_enforced() {
        let json = recipe_with_layer(
            r#"{ "kind": "text", "text": "Hi", "duration": 2.0, "animate": [
                { "param": "scale", "keys": [
                    { "time": 1.0, "value": 1 },
                    { "time": 0.5, "value": 2 },
                    { "time": 9.0, "value": 3 }
                ] }
            ] }"#,
        );
        let issues = issues_of(&json);
        assert!(issues.iter().any(|i| i.message.contains("must come after")), "{issues:?}");
        assert!(issues.iter().any(|i| i.message.contains("outside the layer's")), "{issues:?}");
    }

    #[test]
    fn value_types_and_ranges_are_enforced() {
        let json = recipe_with_layer(
            r#"{ "kind": "text", "text": "Hi", "animate": [
                { "param": "position", "keys": [ { "time": 0, "value": 5 } ] },
                { "param": "opacity", "keys": [ { "time": 0, "value": 1.5 } ] },
                { "param": "fill", "keys": [ { "time": 0, "value": [2, 0, 0, 1] } ] }
            ] }"#,
        );
        let issues = issues_of(&json);
        assert!(issues.iter().any(|i| i.message.contains("[x, y] pairs")), "{issues:?}");
        assert!(issues.iter().any(|i| i.message.contains("outside 0..=1")), "{issues:?}");
        assert!(issues.iter().any(|i| i.path.contains("value") && i.message.contains("outside 0..=1")), "{issues:?}");
    }

    #[test]
    fn solid_cannot_move() {
        let json = recipe_with_layer(r#"{ "kind": "solid", "fill": [0, 0, 0, 1], "position": [10, 10] }"#);
        assert!(issues_of(&json).iter().any(|i| i.message.contains("a solid fills the whole frame")));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let json = MINIMAL.replace(r#""kind": "text""#, r#""kind": "text", "script": "while(1){}""#);
        match parse_recipe(&json) {
            Err(RecipeReject::Syntax(msg)) => assert!(msg.contains("unknown field"), "{msg}"),
            other => panic!("expected Syntax, got {other:?}"),
        }
    }

    #[test]
    fn rejection_display_lists_every_issue() {
        let json = recipe_with_layer(r#"{ "kind": "shape" }"#);
        let err = parse_recipe(&json).err().expect("must be rejected");
        let text = err.to_string();
        assert!(text.contains("problem(s)"), "{text}");
        assert!(text.contains("layers[0]"), "{text}");
    }
}
