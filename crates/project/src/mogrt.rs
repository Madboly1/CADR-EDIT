//! Per-clip Motion Graphics Template (`.mogrt`) state stored in the project model.
//!
//! A `.mogrt` file imported into the project is a [`MediaClip`](crate::MediaClip) carrying its
//! parsed [`MogrtDefinition`]. Each timeline [`TrackItem`](crate::TrackItem) created from that
//! item receives its own [`MogrtClipState`], holding the definition snapshot, per-clip user
//! overrides, and an optional After Effects (`aerender`) baked video cache path. Because
//! `MogrtClipState` lives inside the copy-on-write [`Project`](crate::Project), every edit and
//! reset participates in full Undo/Redo automatically.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use filmcraft_media::mogrt::{MogrtControl, MogrtControlKind, MogrtControlValue, MogrtDefinition, MogrtFontValue, MogrtMenuOption};

/// Per-clip `.mogrt` state stored on a timeline [`TrackItem`](crate::TrackItem).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MogrtClipState {
    /// Parsed `definition.json` snapshot so the clip remains inspectable and renderable offline.
    pub definition: MogrtDefinition,
    /// Per-clip user overrides keyed by [`MogrtControl::id`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, MogrtControlValue>,
    /// Optional path to a transparent video file baked via After Effects (`aerender`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baked_cache_path: Option<String>,
}

impl MogrtClipState {
    /// Create a fresh clip state from a parsed template definition with no overrides.
    pub fn from_definition(definition: MogrtDefinition) -> Self {
        Self { definition, overrides: BTreeMap::new(), baked_cache_path: None }
    }

    /// Look up a control by exact `id`, case-insensitive `id`, or case-insensitive `ui_name`.
    pub fn find_control(&self, key: &str) -> Option<&MogrtControl> {
        self.definition.find_control(key)
    }

    /// Return the effective value for `control_id` (user override if present, else template default).
    pub fn effective_value(&self, control_id: &str) -> Option<MogrtControlValue> {
        if let Some(v) = self.overrides.get(control_id) {
            return Some(v.clone());
        }
        self.definition.find_control(control_id).and_then(MogrtControl::default_value)
    }

    /// Whether the user has overridden `control_id` on this clip.
    pub fn is_overridden(&self, control_id: &str) -> bool {
        self.overrides.contains_key(control_id)
    }

    /// Override the control matching `key` (by `id` or `ui_name`) with `value`, validating and
    /// clamping to the control's bounds. Returns the canonical control `id`.
    pub fn set_override(&mut self, key: &str, value: MogrtControlValue) -> Result<String, String> {
        let ctrl = self.definition.find_control(key).ok_or_else(|| format!("no .mogrt control `{key}`"))?.clone();
        let validated = validate_for_control(&ctrl, value)?;
        let id = ctrl.id.clone();
        if ctrl.default_value().as_ref() == Some(&validated) {
            self.overrides.remove(&id);
        } else {
            self.overrides.insert(id.clone(), validated);
        }
        Ok(id)
    }

    /// Reset a single control override back to its default. Returns whether an override existed.
    pub fn reset_override(&mut self, key: &str) -> Result<bool, String> {
        let ctrl = self.definition.find_control(key).ok_or_else(|| format!("no .mogrt control `{key}`"))?;
        let id = ctrl.id.clone();
        Ok(self.overrides.remove(&id).is_some())
    }

    /// Reset all control overrides back to template defaults.
    pub fn reset_all(&mut self) {
        self.overrides.clear();
    }
}

fn validate_for_control(ctrl: &MogrtControl, value: MogrtControlValue) -> Result<MogrtControlValue, String> {
    match (&ctrl.kind, value) {
        (MogrtControlKind::Group { .. }, _) => Err(format!("control `{}` is a group header and cannot be edited", ctrl.ui_name)),
        (MogrtControlKind::Text { .. }, MogrtControlValue::Text { text, font }) => Ok(MogrtControlValue::Text { text, font }),
        (MogrtControlKind::Text { .. }, MogrtControlValue::StringInfo(text)) => Ok(MogrtControlValue::Text { text, font: None }),
        (MogrtControlKind::Dropdown { options, .. }, MogrtControlValue::Dropdown(idx)) => {
            Ok(MogrtControlValue::Dropdown(if options.is_empty() { 0 } else { idx.min(options.len() - 1) }))
        }
        (MogrtControlKind::Slider { min, max, .. }, MogrtControlValue::Number(n)) => {
            if !n.is_finite() {
                return Err(format!("control `{}` requires a finite number", ctrl.ui_name));
            }
            let clamped = match (*min, *max) {
                (Some(lo), Some(hi)) => n.clamp(lo.min(hi), lo.max(hi)),
                (Some(lo), None) => n.max(lo),
                (None, Some(hi)) => n.min(hi),
                (None, None) => n,
            };
            Ok(MogrtControlValue::Number(clamped))
        }
        (MogrtControlKind::Checkbox { .. }, MogrtControlValue::Bool(b)) => Ok(MogrtControlValue::Bool(b)),
        (MogrtControlKind::Color { .. }, MogrtControlValue::Color(c)) => {
            if !c.into_iter().all(f32::is_finite) {
                return Err(format!("control `{}` requires finite RGBA channels", ctrl.ui_name));
            }
            Ok(MogrtControlValue::Color([c[0].clamp(0.0, 1.0), c[1].clamp(0.0, 1.0), c[2].clamp(0.0, 1.0), c[3].clamp(0.0, 1.0)]))
        }
        (MogrtControlKind::Point { .. }, MogrtControlValue::Point([x, y])) => {
            if !x.is_finite() || !y.is_finite() {
                return Err(format!("control `{}` requires finite X/Y coordinates", ctrl.ui_name));
            }
            Ok(MogrtControlValue::Point([x.clamp(-100_000.0, 100_000.0), y.clamp(-100_000.0, 100_000.0)]))
        }
        (MogrtControlKind::StringInfo { .. }, MogrtControlValue::StringInfo(s)) => Ok(MogrtControlValue::StringInfo(s)),
        (MogrtControlKind::StringInfo { .. }, MogrtControlValue::Text { text, .. }) => Ok(MogrtControlValue::StringInfo(text)),
        _ => Err(format!("value type does not match control `{}`", ctrl.ui_name)),
    }
}

/// Parse a loosely typed JSON value (from engine commands / MCP / UI) into a [`MogrtControlValue`]
/// matching `ctrl`.
pub fn json_to_control_value(ctrl: &MogrtControl, v: &Value) -> Result<MogrtControlValue, String> {
    match &ctrl.kind {
        MogrtControlKind::Group { .. } => Err(format!("control `{}` is a group header", ctrl.ui_name)),
        MogrtControlKind::Text { .. } => {
            if let Some(s) = v.as_str() {
                return Ok(MogrtControlValue::Text { text: s.to_string(), font: None });
            }
            if let Some(obj) = v.as_object() {
                let text = obj.get("text").and_then(Value::as_str).ok_or_else(|| "text control object needs string `text`".to_string())?.to_string();
                let font = obj.get("font").and_then(|f| serde_json::from_value::<MogrtFontValue>(f.clone()).ok());
                return Ok(MogrtControlValue::Text { text, font });
            }
            Err("text control expects a string or {\"text\", \"font\"?}".into())
        }
        MogrtControlKind::Dropdown { options, .. } => {
            if let Some(i) = v.as_u64() {
                return Ok(MogrtControlValue::Dropdown(i as usize));
            }
            if let Some(name) = v.as_str() {
                let idx = options
                    .iter()
                    .position(|o| o.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| format!("unknown menu option `{name}` for `{}`", ctrl.ui_name))?;
                return Ok(MogrtControlValue::Dropdown(idx));
            }
            Err("dropdown control expects an option index or option name".into())
        }
        MogrtControlKind::Slider { .. } => {
            let n = v.as_f64().ok_or_else(|| "slider control expects a number".to_string())?;
            Ok(MogrtControlValue::Number(n))
        }
        MogrtControlKind::Checkbox { .. } => {
            let b = v.as_bool().ok_or_else(|| "checkbox control expects a boolean".to_string())?;
            Ok(MogrtControlValue::Bool(b))
        }
        MogrtControlKind::Color { .. } => parse_color_json(v).map(MogrtControlValue::Color),
        MogrtControlKind::Point { .. } => parse_point_json(v).map(MogrtControlValue::Point),
        MogrtControlKind::StringInfo { .. } => {
            let s = v.as_str().ok_or_else(|| "string control expects a string".to_string())?;
            Ok(MogrtControlValue::StringInfo(s.to_string()))
        }
    }
}

/// Convert a [`MogrtControlValue`] to a concise JSON value for `mogrt.inspect` / UI / MCP.
pub fn control_value_to_json(v: &MogrtControlValue) -> Value {
    match v {
        MogrtControlValue::Text { text, font } => match font {
            Some(f) => serde_json::json!({ "text": text, "font": f }),
            None => Value::String(text.clone()),
        },
        MogrtControlValue::Dropdown(i) => serde_json::json!(i),
        MogrtControlValue::Number(n) => serde_json::json!(n),
        MogrtControlValue::Bool(b) => serde_json::json!(b),
        MogrtControlValue::Color(c) => serde_json::json!(c),
        MogrtControlValue::Point(p) => serde_json::json!({ "x": p[0], "y": p[1] }),
        MogrtControlValue::StringInfo(s) => Value::String(s.clone()),
    }
}

fn parse_color_json(v: &Value) -> Result<[f32; 4], String> {
    if let Some(arr) = v.as_array()
        && arr.len() >= 3
    {
        let r = arr.first().and_then(Value::as_f64).ok_or_else(|| "invalid red channel".to_string())?;
        let g = arr.get(1).and_then(Value::as_f64).ok_or_else(|| "invalid green channel".to_string())?;
        let b = arr.get(2).and_then(Value::as_f64).ok_or_else(|| "invalid blue channel".to_string())?;
        let a = arr.get(3).and_then(Value::as_f64).unwrap_or(1.0);
        if ![r, g, b, a].into_iter().all(f64::is_finite) {
            return Err("color channels must be finite".into());
        }
        let scale = if r > 1.0 || g > 1.0 || b > 1.0 || a > 1.0 { 255.0 } else { 1.0 };
        return Ok([
            (r / scale).clamp(0.0, 1.0) as f32,
            (g / scale).clamp(0.0, 1.0) as f32,
            (b / scale).clamp(0.0, 1.0) as f32,
            (a / scale).clamp(0.0, 1.0) as f32,
        ]);
    }
    if let Some(s) = v.as_str() {
        let hex = s.trim().strip_prefix('#').ok_or_else(|| "hex color must start with `#`".to_string())?;
        if hex.len() != 6 && hex.len() != 8 {
            return Err("hex color must be #rrggbb or #rrggbbaa".into());
        }
        let byte = |i: usize| {
            hex.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .map(|b| f32::from(b) / 255.0)
                .ok_or_else(|| "invalid hex color digit".to_string())
        };
        let r = byte(0)?;
        let g = byte(2)?;
        let b = byte(4)?;
        let a = if hex.len() == 8 { byte(6)? } else { 1.0 };
        return Ok([r, g, b, a]);
    }
    Err("color control expects [r, g, b, a] or #rrggbb".into())
}

fn parse_point_json(v: &Value) -> Result<[f64; 2], String> {
    if let Some(arr) = v.as_array()
        && arr.len() >= 2
    {
        let x = arr.first().and_then(Value::as_f64).ok_or_else(|| "invalid point x".to_string())?;
        let y = arr.get(1).and_then(Value::as_f64).ok_or_else(|| "invalid point y".to_string())?;
        return Ok([x, y]);
    }
    if let Some(obj) = v.as_object() {
        let x = obj.get("x").and_then(Value::as_f64).ok_or_else(|| "point object needs numeric `x`".to_string())?;
        let y = obj.get("y").and_then(Value::as_f64).ok_or_else(|| "point object needs numeric `y`".to_string())?;
        return Ok([x, y]);
    }
    Err("point control expects [x, y] or {\"x\", \"y\"}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_def() -> MogrtDefinition {
        MogrtDefinition {
            capsule_name: "Test Title".into(),
            width: 1920,
            height: 1080,
            controls: vec![
                MogrtControl {
                    id: "text".into(),
                    ui_name: "Source Text".into(),
                    control_type: 6,
                    group: None,
                    kind: MogrtControlKind::Text { default_text: "Default".into(), font: MogrtFontValue::default() },
                },
                MogrtControl {
                    id: "size".into(),
                    ui_name: "Text Size".into(),
                    control_type: 2,
                    group: None,
                    kind: MogrtControlKind::Slider { default_value: 72.0, min: Some(10.0), max: Some(200.0) },
                },
                MogrtControl {
                    id: "anim".into(),
                    ui_name: "Animation".into(),
                    control_type: 13,
                    group: None,
                    kind: MogrtControlKind::Dropdown {
                        options: vec![MogrtMenuOption { name: "Slide Up".into(), value: 0 }, MogrtMenuOption { name: "Smooth Zoom".into(), value: 1 }],
                        default_index: 0,
                    },
                },
            ],
            ..MogrtDefinition::default()
        }
    }

    #[test]
    fn overrides_clamp_and_reset_cleanly() {
        let mut st = MogrtClipState::from_definition(sample_def());
        assert_eq!(st.effective_value("text"), Some(MogrtControlValue::Text { text: "Default".into(), font: Some(MogrtFontValue::default()) }));

        st.set_override("Source Text", MogrtControlValue::Text { text: "Updated".into(), font: None }).unwrap();
        assert!(st.is_overridden("text"));
        assert_eq!(st.effective_value("text"), Some(MogrtControlValue::Text { text: "Updated".into(), font: None }));

        // Slider clamps to [10, 200]
        st.set_override("size", MogrtControlValue::Number(999.0)).unwrap();
        assert_eq!(st.effective_value("size"), Some(MogrtControlValue::Number(200.0)));

        // Non-finite rejected
        assert!(st.set_override("size", MogrtControlValue::Number(f64::NAN)).is_err());

        // Dropdown by name
        let anim_ctl = st.find_control("anim").unwrap().clone();
        let val = json_to_control_value(&anim_ctl, &serde_json::json!("Smooth Zoom")).unwrap();
        st.set_override("anim", val).unwrap();
        assert_eq!(st.effective_value("anim"), Some(MogrtControlValue::Dropdown(1)));

        // Reset single and all
        assert!(st.reset_override("size").unwrap());
        assert!(!st.is_overridden("size"));
        st.reset_all();
        assert!(st.overrides.is_empty());

        // Serde round-trip
        st.set_override("text", MogrtControlValue::Text { text: "Persisted".into(), font: None }).unwrap();
        let json = serde_json::to_string(&st).unwrap();
        let back: MogrtClipState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, st);
    }
}
