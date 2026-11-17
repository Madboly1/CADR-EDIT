//! Engine commands and After Effects (`aerender`) bridge for Motion Graphics Template (`.mogrt`)
//! clips.
//!
//! | Command | Kind | Purpose |
//! |---|---|---|
//! | `mogrt.inspect` | query | Read template metadata, controls, overrides, and AE cache status |
//! | `mogrt.setControl` | edit | Override a `.mogrt` control value (full Undo/Redo + drag merge) |
//! | `mogrt.resetControl` | edit | Reset one control (or all controls) to template defaults |
//! | `mogrt.renderAe` | edit | Bake a `.mogrt` containing `project.aegraphic` via `aerender` |
//! | `mogrt.clearAeCache` | edit | Clear the baked AE cache and return to the native renderer |

use std::sync::OnceLock;

use filmcraft_project::mogrt::{MogrtControl, MogrtControlKind, control_value_to_json, json_to_control_value};
use filmcraft_project::{ClipId, MediaRef, TrackItem};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, always, bad, bool_p, has_seq, str_p, u64_p};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled, run, journal: true, named: NamedKeys::NONE }
}

fn query(id: &'static str, label: &'static str, params: &'static str, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal: false, named: NamedKeys::NONE }
}

/// Find the target `.mogrt` clip: explicit `clip` param, else first selected `.mogrt` clip, else
/// topmost `.mogrt` clip under the playhead.
pub fn target_mogrt_clip(s: &Session, p: &Value) -> Option<ClipId> {
    let seq = s.active_sequence()?;
    if let Some(id) = u64_p(p, "clip").map(ClipId) {
        return seq.find_item(id).filter(|(_, it)| it.mogrt.is_some()).map(|(_, it)| it.id);
    }
    if let Some(id) = s.state.selection.iter().copied().find(|c| seq.find_item(*c).is_some_and(|(_, it)| it.mogrt.is_some())) {
        return Some(id);
    }
    let ph = s.playhead();
    seq.video_tracks.iter().rev().filter(|t| t.enabled).find_map(|t| t.item_at(ph).filter(|it| it.mogrt.is_some()).map(|it| it.id))
}

fn has_mogrt(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    target_mogrt_clip(s, &Value::Null).map(|_| ()).ok_or_else(|| "select a Motion Graphics Template (.mogrt) clip".into())
}

// ------------------------------------------------------------------------------------------------
// After Effects (aerender) detection
// ------------------------------------------------------------------------------------------------

/// Locate an installed Adobe After Effects `aerender` binary on the host system.
#[cfg(not(target_arch = "wasm32"))]
pub fn detect_aerender() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("AERENDER_PATH") {
        let p = std::path::PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }
    let bin = if cfg!(windows) { "aerender.exe" } else { "aerender" };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    detect_aerender_standard_dirs(bin)
}

#[cfg(target_arch = "wasm32")]
pub fn detect_aerender() -> Option<std::path::PathBuf> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn detect_aerender_standard_dirs(bin: &str) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let adobe_dir = std::path::Path::new(r"C:\Program Files\Adobe");
        if let Ok(entries) = std::fs::read_dir(adobe_dir) {
            let mut dirs: Vec<std::path::PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("Adobe After Effects")))
                .collect();
            dirs.sort();
            for dir in dirs.into_iter().rev() {
                let candidate = dir.join("Support Files").join(bin);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let apps = std::path::Path::new("/Applications");
        if let Ok(entries) = std::fs::read_dir(apps) {
            let mut dirs: Vec<std::path::PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("Adobe After Effects")))
                .collect();
            dirs.sort();
            for dir in dirs.into_iter().rev() {
                let candidate = dir.join(bin);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let _ = bin;
    None
}

/// Cached check for whether `aerender` is available on this system (safe to call from the UI).
pub fn aerender_available() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| detect_aerender().is_some())
}

// ------------------------------------------------------------------------------------------------
// Command implementations
// ------------------------------------------------------------------------------------------------

fn control_to_json(ctrl: &MogrtControl, it: &TrackItem) -> Value {
    let mogrt = it.mogrt.as_deref();
    let eff = mogrt.and_then(|m| m.effective_value(&ctrl.id));
    let overridden = mogrt.is_some_and(|m| m.is_overridden(&ctrl.id));
    let mut obj = json!({
        "id": ctrl.id,
        "uiName": ctrl.ui_name,
        "type": ctrl.control_type,
        "group": ctrl.group,
        "overridden": overridden,
        "value": eff.as_ref().map(control_value_to_json).unwrap_or(Value::Null),
    });
    match &ctrl.kind {
        MogrtControlKind::Dropdown { options, .. } => {
            obj["options"] = json!(options.iter().map(|o| &o.name).collect::<Vec<_>>());
        }
        MogrtControlKind::Slider { min, max, .. } => {
            obj["min"] = json!(min);
            obj["max"] = json!(max);
        }
        MogrtControlKind::StringInfo { hidden_by_default, .. } => {
            obj["hidden"] = json!(hidden_by_default);
        }
        _ => {}
    }
    obj
}

fn inspect_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let clip = target_mogrt_clip(s, p).ok_or_else(|| bad("mogrt.inspect", "no .mogrt clip selected"))?;
    let seq = s.active_sequence().ok_or_else(|| bad("mogrt.inspect", "no active sequence"))?;
    let (_, it) = seq.find_item(clip).ok_or_else(|| bad("mogrt.inspect", "clip not found"))?;
    let mogrt = it.mogrt.as_deref().ok_or_else(|| bad("mogrt.inspect", "clip is not a .mogrt template"))?;
    let def = &mogrt.definition;
    let controls: Vec<Value> = def.flat_controls().into_iter().map(|c| control_to_json(c, it)).collect();
    Ok(json!({
        "clip": clip.0,
        "capsuleName": def.capsule_name,
        "width": def.width,
        "height": def.height,
        "hasAegraphic": def.has_aegraphic,
        "aerenderAvailable": detect_aerender().is_some(),
        "bakedCachePath": mogrt.baked_cache_path,
        "controls": controls,
    }))
}

fn set_control_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "mogrt.setControl";
    let clip = target_mogrt_clip(s, p).ok_or_else(|| bad(ID, "no .mogrt clip selected"))?;
    let key = str_p(p, "control").ok_or_else(|| bad(ID, "need `control` (id or name)"))?.to_string();
    let raw_val = p.get("value").ok_or_else(|| bad(ID, "need `value`"))?.clone();

    // Validate control and value before touching the project.
    let (ctrl_id, parsed_val) = {
        let seq = s.active_sequence().ok_or_else(|| bad(ID, "no active sequence"))?;
        let (_, it) = seq.find_item(clip).ok_or_else(|| bad(ID, "clip not found"))?;
        let mogrt = it.mogrt.as_deref().ok_or_else(|| bad(ID, "clip is not a .mogrt template"))?;
        let ctrl = mogrt.find_control(&key).ok_or_else(|| bad(ID, format!("no .mogrt control `{key}`")))?;
        let parsed = json_to_control_value(ctrl, &raw_val).map_err(|e| bad(ID, e))?;
        (ctrl.id.clone(), parsed)
    };

    let merge = bool_p(p, "merge").unwrap_or(false).then(|| format!("mogrt.setControl:{}:{ctrl_id}", clip.0));
    if bool_p(p, "begin").unwrap_or(false) {
        s.history.merge_key = None;
    }

    let applied_id = s.edit_sequence_as("Change Template Property", merge.as_deref(), |q, _, _| {
        let (_, it) = q.find_item_mut(clip).ok_or(filmcraft_edit::EditError::NoItem(clip))?;
        let mogrt = it.mogrt.as_deref_mut().ok_or_else(|| bad(ID, "clip is not a .mogrt template"))?;
        mogrt.set_override(&ctrl_id, parsed_val.clone()).map_err(|e| bad(ID, e))
    })?;

    Ok(json!({
        "clip": clip.0,
        "control": applied_id,
        "value": control_value_to_json(&parsed_val),
    }))
}

fn reset_control_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "mogrt.resetControl";
    let clip = target_mogrt_clip(s, p).ok_or_else(|| bad(ID, "no .mogrt clip selected"))?;
    let control = str_p(p, "control").map(str::to_string);
    let label = if control.is_some() { "Reset Template Property" } else { "Reset All Template Properties" };

    s.edit_sequence(label, |q, _, _| {
        let (_, it) = q.find_item_mut(clip).ok_or(filmcraft_edit::EditError::NoItem(clip))?;
        let mogrt = it.mogrt.as_deref_mut().ok_or_else(|| bad(ID, "clip is not a .mogrt template"))?;
        match &control {
            Some(key) => {
                mogrt.reset_override(key).map_err(|e| bad(ID, e))?;
            }
            None => mogrt.reset_all(),
        }
        Ok(())
    })?;

    Ok(json!({ "clip": clip.0, "reset": control.unwrap_or_else(|| "all".into()) }))
}

fn render_ae_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "mogrt.renderAe";
    let clip = target_mogrt_clip(s, p).ok_or_else(|| bad(ID, "no .mogrt clip selected"))?;

    let (source_path, has_aegraphic, capsule_name) = {
        let seq = s.active_sequence().ok_or_else(|| bad(ID, "no active sequence"))?;
        let (_, it) = seq.find_item(clip).ok_or_else(|| bad(ID, "clip not found"))?;
        let mogrt = it.mogrt.as_deref().ok_or_else(|| bad(ID, "clip is not a .mogrt template"))?;
        let path = s
            .project
            .resolve_media(it.item)
            .and_then(|(_, m, _)| match &m.media {
                MediaRef::File { path } => Some(path.clone()),
                MediaRef::Generator(_) => None,
            })
            .ok_or_else(|| bad(ID, "template has no source .mogrt file on disk"))?;
        (path, mogrt.definition.has_aegraphic, mogrt.definition.capsule_name.clone())
    };

    if !has_aegraphic {
        return Err(bad(ID, "this .mogrt does not contain a project.aegraphic After Effects project"));
    }

    #[cfg(target_arch = "wasm32")]
    {
        let _ = (source_path, capsule_name);
        Err(EngineError::Other("After Effects (aerender) baking is not available in the web build".into()))
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let aerender = detect_aerender().ok_or_else(|| {
            EngineError::Other("After Effects `aerender` was not found on this system (set AERENDER_PATH or install After Effects)".into())
        })?;
        let bytes = s.services.read_file(&source_path).map_err(|e| EngineError::Other(format!("{source_path}: {e}")))?;
        let out_path = bake_via_aerender(&aerender, &bytes, clip, &capsule_name)?;
        s.media.allow(&out_path);
        s.edit_sequence("Bake Template via After Effects", |q, _, _| {
            let (_, it) = q.find_item_mut(clip).ok_or(filmcraft_edit::EditError::NoItem(clip))?;
            let mogrt = it.mogrt.as_deref_mut().ok_or_else(|| bad(ID, "clip is not a .mogrt template"))?;
            mogrt.baked_cache_path = Some(out_path.clone());
            Ok(())
        })?;
        Ok(json!({ "clip": clip.0, "bakedCachePath": out_path }))
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn bake_via_aerender(aerender: &std::path::Path, mogrt_bytes: &[u8], clip: ClipId, capsule_name: &str) -> Result<String> {
    let entries = filmcraft_media::mogrt::extract_all_safe(mogrt_bytes).map_err(|e| EngineError::Other(format!("extract .mogrt: {e}")))?;
    let base_dir = crate::private_temp_dir().map_err(|e| EngineError::Other(format!("temp dir: {e}")))?.join("cadr-mogrt-ae").join(format!("clip-{}", clip.0));
    std::fs::create_dir_all(&base_dir).map_err(|e| EngineError::Other(format!("create cache dir: {e}")))?;

    let mut aegraphic_path: Option<std::path::PathBuf> = None;
    for (rel, data) in &entries {
        let dest = base_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EngineError::Other(format!("create dir: {e}")))?;
        }
        std::fs::write(&dest, data).map_err(|e| EngineError::Other(format!("write {rel}: {e}")))?;
        if rel.to_ascii_lowercase().ends_with(".aegraphic") {
            aegraphic_path = Some(dest);
        }
    }

    let aegraphic = aegraphic_path.ok_or_else(|| EngineError::Other("project.aegraphic missing from .mogrt archive".into()))?;
    // Some `.aegraphic` files are nested ZIP packages containing a `.aep` project file; unpack if so.
    let project_arg = unpack_nested_aep(&aegraphic, &base_dir).unwrap_or(aegraphic);

    let safe_stem: String = capsule_name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(48).collect();
    let stem = if safe_stem.is_empty() { format!("mogrt_{}", clip.0) } else { safe_stem };
    let output_path = base_dir.join(format!("{stem}_baked.mov"));

    let status = std::process::Command::new(aerender)
        .arg("-project")
        .arg(&project_arg)
        .arg("-OMtemplate")
        .arg("Lossless with Alpha")
        .arg("-output")
        .arg(&output_path)
        .output()
        .map_err(|e| EngineError::Other(format!("failed to launch aerender: {e}")))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        let stdout = String::from_utf8_lossy(&status.stdout);
        let detail = if !stderr.trim().is_empty() { stderr.trim() } else { stdout.trim() };
        return Err(EngineError::Other(format!("aerender exited with {}: {detail}", status.status)));
    }
    if !output_path.is_file() {
        return Err(EngineError::Other(format!("aerender succeeded but did not produce {}", output_path.display())));
    }
    Ok(output_path.to_string_lossy().to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn unpack_nested_aep(aegraphic: &std::path::Path, base_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let raw = std::fs::read(aegraphic).ok()?;
    if !raw.starts_with(b"PK\x03\x04") {
        return None;
    }
    let nested = filmcraft_media::mogrt::extract_all_safe(&raw).ok()?;
    let unpack_dir = base_dir.join("aegraphic");
    let mut found_aep = None;
    for (rel, data) in nested {
        let dest = unpack_dir.join(&rel);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&dest, data).is_ok() && rel.to_ascii_lowercase().ends_with(".aep") {
            found_aep = Some(dest);
        }
    }
    found_aep
}

fn clear_ae_cache_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "mogrt.clearAeCache";
    let clip = target_mogrt_clip(s, p).ok_or_else(|| bad(ID, "no .mogrt clip selected"))?;
    s.edit_sequence("Clear After Effects Template Cache", |q, _, _| {
        let (_, it) = q.find_item_mut(clip).ok_or(filmcraft_edit::EditError::NoItem(clip))?;
        let mogrt = it.mogrt.as_deref_mut().ok_or_else(|| bad(ID, "clip is not a .mogrt template"))?;
        mogrt.baked_cache_path = None;
        Ok(())
    })?;
    Ok(json!({ "clip": clip.0, "bakedCachePath": Value::Null }))
}

/// All `.mogrt` commands registered in the engine command table.
pub fn commands() -> Vec<CommandSpec> {
    vec![
        query("mogrt.inspect", "Inspect Motion Graphics Template", r#"{"clip":id?}"#, inspect_cmd),
        spec(
            "mogrt.setControl",
            "Set Motion Graphics Template Control",
            r#"{"clip":id?,"control":str,"value":any,"merge":bool?,"begin":bool?}"#,
            has_mogrt,
            set_control_cmd,
        ),
        spec("mogrt.resetControl", "Reset Motion Graphics Template Control", r#"{"clip":id?,"control":str?}"#, has_mogrt, reset_control_cmd),
        spec("mogrt.renderAe", "Render Template via After Effects (aerender)", r#"{"clip":id?}"#, has_mogrt, render_ae_cmd),
        spec("mogrt.clearAeCache", "Clear After Effects Template Cache", r#"{"clip":id?}"#, has_mogrt, clear_ae_cache_cmd),
    ]
}

#[cfg(test)]
mod tests {
    use filmcraft_media::mogrt::build_zip;
    use filmcraft_project::{ClipId, ItemId};
    use serde_json::json;

    use crate::Session;
    use crate::media_test_util::tmp_dir;

    fn sample_mogrt_bytes(with_aegraphic: bool) -> Vec<u8> {
        let def = serde_json::to_vec(&json!({
            "capsuleName": "Promo Title",
            "sourceInfoLocalized": { "en_US": { "framesize": [1280, 720] } },
            "duration": 5.0,
            "framerate": 30.0,
            "clientControls": [
                {
                    "id": "grp_main",
                    "type": 10,
                    "uiName": "Main",
                    "clientControls": [
                        {
                            "id": "src_text",
                            "type": 6,
                            "uiName": "Source Text",
                            "value": { "textEditValue": "Initial Title" }
                        },
                        {
                            "id": "txt_color",
                            "type": 4,
                            "uiName": "Text Color",
                            "value": [1.0, 1.0, 1.0, 1.0]
                        },
                        {
                            "id": "txt_size",
                            "type": 2,
                            "uiName": "Text Size",
                            "value": 72.0,
                            "min": 16.0,
                            "max": 200.0
                        },
                        {
                            "id": "txt_pos",
                            "type": 5,
                            "uiName": "Text Position",
                            "value": { "x": 640.0, "y": 360.0 }
                        }
                    ]
                },
                {
                    "id": "anim",
                    "type": 13,
                    "uiName": "Animation Preset",
                    "value": 1,
                    "menucontent": [
                        { "name": "None", "value": 0 },
                        { "name": "Slide Up", "value": 1 },
                        { "name": "Smooth Zoom", "value": 2 }
                    ]
                }
            ]
        }))
        .unwrap();
        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([20, 140, 220, 255]));
        let mut cur = std::io::Cursor::new(&mut png);
        img.write_to(&mut cur, image::ImageFormat::Png).unwrap();
        if with_aegraphic {
            build_zip(&[("definition.json", &def), ("thumb.png", &png), ("project.aegraphic", b"dummy")], true)
        } else {
            build_zip(&[("definition.json", &def), ("thumb.png", &png)], true)
        }
    }

    #[test]
    fn import_place_override_undo_redo_and_render_mogrt() {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"name": "Main", "width": 1280, "height": 720, "fps": 30})).unwrap();

        let dir = tmp_dir("engine-mogrt-e2e");
        let path = dir.join("promo_title.mogrt");
        std::fs::write(&path, sample_mogrt_bytes(false)).unwrap();

        let imp = s.execute("file.import", json!({"paths": [path.to_string_lossy()]})).unwrap();
        let item_id = ItemId(imp["items"][0].as_u64().unwrap());
        assert!(s.project.mogrt_templates.contains_key(&item_id));
        assert_eq!(s.project.item(item_id).unwrap().type_label(), "Motion Graphics Template");

        // Place onto timeline
        let placed = s.execute("timeline.place", json!({"item": item_id.0, "time": 0})).unwrap();
        let clip_id = ClipId(placed["clips"][0].as_u64().unwrap());
        s.state.selection = vec![clip_id];

        // Inspect default controls
        let info = s.execute("mogrt.inspect", json!({"clip": clip_id.0})).unwrap();
        assert_eq!(info["capsuleName"], "Promo Title");
        assert_eq!(info["hasAegraphic"], false);
        assert_eq!(info["controls"][0]["value"]["text"], "Initial Title");

        // Override Source Text, Color, Size, Position, Dropdown with full Undo/Redo
        s.execute("mogrt.setControl", json!({"clip": clip_id.0, "control": "Source Text", "value": "CADR Live"})).unwrap();
        s.execute("mogrt.setControl", json!({"clip": clip_id.0, "control": "txt_color", "value": "#ff3300"})).unwrap();
        s.execute("mogrt.setControl", json!({"clip": clip_id.0, "control": "txt_size", "value": 96.0})).unwrap();
        s.execute("mogrt.setControl", json!({"clip": clip_id.0, "control": "txt_pos", "value": {"x": 500.0, "y": 300.0}})).unwrap();
        s.execute("mogrt.setControl", json!({"clip": clip_id.0, "control": "anim", "value": "Smooth Zoom"})).unwrap();

        let after = s.execute("mogrt.inspect", json!({"clip": clip_id.0})).unwrap();
        assert_eq!(after["controls"][0]["value"], "CADR Live");
        assert_eq!(after["controls"][4]["value"], 2);

        // Undo reverts dropdown override, Redo restores it
        s.execute("edit.undo", json!({})).unwrap();
        let undone = s.execute("mogrt.inspect", json!({"clip": clip_id.0})).unwrap();
        assert_eq!(undone["controls"][4]["value"], 1);
        s.execute("edit.redo", json!({})).unwrap();
        let redone = s.execute("mogrt.inspect", json!({"clip": clip_id.0})).unwrap();
        assert_eq!(redone["controls"][4]["value"], 2);

        // Native frame render produces visible non-empty alpha pixels
        let seq_id = s.state.active_sequence.unwrap();
        let provider = s.media.provider(s.project.clone(), s.services.clone());
        let img = filmcraft_render::render_sequence(
            &s.project,
            seq_id,
            filmcraft_time::Tick::from_seconds_f64(2.0),
            filmcraft_render::RenderOptions::default(),
            &provider,
        );
        let lit = img.px.as_chunks::<4>().0.iter().filter(|p| p[3] > 0.1).count();
        assert!(lit > 100, "expected rendered .mogrt text pixels, got {lit}");

        // Reset all overrides and verify Undo restores them
        s.execute("mogrt.resetControl", json!({"clip": clip_id.0})).unwrap();
        let reset = s.execute("mogrt.inspect", json!({"clip": clip_id.0})).unwrap();
        assert_eq!(reset["controls"][0]["value"]["text"], "Initial Title");

        // Rendering AE on a template without project.aegraphic fails cleanly
        assert!(s.execute("mogrt.renderAe", json!({"clip": clip_id.0})).is_err());
    }
}
