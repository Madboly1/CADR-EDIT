//! Native Motion Graphics Template (`.mogrt`) archive reader, `definition.json` parser, and
//! [`MediaSource`] implementation.
//!
//! A `.mogrt` file is a ZIP archive containing:
//! - `definition.json`: template metadata (`capsuleName`, `sourceInfoLocalized.*.framesize`,
//!   `duration`, `framerate`, and `clientControls`).
//! - `thumb.png` (optional): preview thumbnail used for media browser, project panel, and timeline
//!   thumbnails.
//! - `project.aegraphic` (optional): After Effects graphics package, enabling an optional
//!   `aerender` bake workflow when After Effects is installed.
//! - Optional companion media assets inside the archive.
//!
//! Every field and byte offset from the archive is treated as hostile input: entry counts,
//! decompressed sizes, dimensions, durations, and recursion depths are strictly bounded.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{FrameRequest, MediaError, MediaInfo, MediaKind, MediaSource, Result, VideoStreamInfo, validate_audio_buffer_shape};

/// Maximum number of entries accepted from a `.mogrt` ZIP archive.
pub const MAX_ZIP_ENTRIES: usize = 4096;
/// Maximum decompressed bytes for `definition.json` (16 MiB).
pub const MAX_DEFINITION_BYTES: usize = 16 * 1024 * 1024;
/// Maximum decompressed bytes for `thumb.png` (32 MiB).
pub const MAX_THUMB_BYTES: usize = 32 * 1024 * 1024;
/// Maximum decompressed bytes for any extracted companion asset (64 MiB).
pub const MAX_ASSET_BYTES: usize = 64 * 1024 * 1024;
/// Maximum recursion depth for nested `clientControls` groups.
const MAX_CONTROL_DEPTH: usize = 16;
/// Maximum total controls accepted from a single `definition.json`.
const MAX_CONTROLS: usize = 1024;
/// Maximum template canvas side in pixels.
const MAX_TEMPLATE_SIDE: u32 = 16_384;
/// Default template duration when omitted or invalid (5 seconds).
const DEFAULT_DURATION_SECS: f64 = 5.0;
/// Maximum template duration accepted (1 hour).
const MAX_DURATION_SECS: f64 = 3600.0;

// ------------------------------------------------------------------------------------------------
// Data model
// ------------------------------------------------------------------------------------------------

/// Font formatting attributes embedded in a Text/Source Text (`type: 6`) control's
/// `fontEditValue`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MogrtFontValue {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub font_family: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub font_style: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub faux_bold: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub faux_italic: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub all_caps: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub small_caps: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracking: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leading: Option<f32>,
}

/// One option inside a Dropdown/Menu (`type: 13`) control's `menucontent`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MogrtMenuOption {
    pub name: String,
    pub value: i64,
}

/// Control kind parsed from `definition.json`'s `clientControls` array.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MogrtControlKind {
    /// Collapsible group header (`type: 10`), optionally containing nested child controls.
    Group {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<MogrtControl>,
    },
    /// Editable Source Text (`type: 6`) with optional `fontEditValue`.
    Text {
        default_text: String,
        #[serde(default)]
        font: MogrtFontValue,
    },
    /// Dropdown menu (`type: 13`) with `menucontent` choices and default selected index.
    Dropdown {
        options: Vec<MogrtMenuOption>,
        default_index: usize,
    },
    /// Numeric slider / scalar (`type: 2`) with optional range limits.
    Slider {
        default_value: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
    },
    /// Boolean checkbox (`type: 1` or `type: 3`).
    Checkbox {
        default_value: bool,
    },
    /// RGBA color (`type: 4`), normalized to `0.0..=1.0`.
    Color {
        default_rgba: [f32; 4],
    },
    /// 2D point / position (`type: 5`), `[x, y]`.
    Point {
        default_point: [f64; 2],
    },
    /// Read-only / informational string (`type: 8`).
    StringInfo {
        text: String,
        /// True when empty/whitespace-only or recognized as a promotional/credit banner.
        #[serde(default)]
        hidden_by_default: bool,
    },
}

/// One control definition parsed from `definition.json`'s `clientControls`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MogrtControl {
    /// Stable control identifier from `definition.json` (or synthesized if missing).
    pub id: String,
    /// Human-readable label (`uiName`).
    pub ui_name: String,
    /// Raw numeric `type` code from `definition.json` (`10`, `6`, `13`, `2`, `4`, `5`, `8`, …).
    pub control_type: u32,
    /// Optional parent group title when controls are listed flat after a `type: 10` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Typed kind and default value.
    pub kind: MogrtControlKind,
}

impl Default for MogrtControl {
    fn default() -> Self {
        Self {
            id: String::new(),
            ui_name: String::new(),
            control_type: 8,
            group: None,
            kind: MogrtControlKind::StringInfo { text: String::new(), hidden_by_default: true },
        }
    }
}

impl MogrtControl {
    /// The control's default [`MogrtControlValue`], or `None` for group headers.
    pub fn default_value(&self) -> Option<MogrtControlValue> {
        match &self.kind {
            MogrtControlKind::Group { .. } => None,
            MogrtControlKind::Text { default_text, font } => Some(MogrtControlValue::Text { text: default_text.clone(), font: Some(font.clone()) }),
            MogrtControlKind::Dropdown { default_index, .. } => Some(MogrtControlValue::Dropdown(*default_index)),
            MogrtControlKind::Slider { default_value, .. } => Some(MogrtControlValue::Number(*default_value)),
            MogrtControlKind::Checkbox { default_value } => Some(MogrtControlValue::Bool(*default_value)),
            MogrtControlKind::Color { default_rgba } => Some(MogrtControlValue::Color(*default_rgba)),
            MogrtControlKind::Point { default_point } => Some(MogrtControlValue::Point(*default_point)),
            MogrtControlKind::StringInfo { text, .. } => Some(MogrtControlValue::StringInfo(text.clone())),
        }
    }
}

/// Per-clip overridden value for a `.mogrt` control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum MogrtControlValue {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        font: Option<MogrtFontValue>,
    },
    Dropdown(usize),
    Number(f64),
    Bool(bool),
    Color([f32; 4]),
    Point([f64; 2]),
    StringInfo(String),
}

/// Parsed `.mogrt` template metadata extracted from `definition.json` and the archive manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MogrtDefinition {
    pub capsule_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub capsule_id: String,
    pub width: u32,
    pub height: u32,
    pub duration: Tick,
    pub frame_rate: FrameRate,
    /// True when `project.aegraphic` is present inside the `.mogrt` archive.
    #[serde(default)]
    pub has_aegraphic: bool,
    pub controls: Vec<MogrtControl>,
}

impl Default for MogrtDefinition {
    fn default() -> Self {
        Self {
            capsule_name: String::new(),
            capsule_id: String::new(),
            width: 1920,
            height: 1080,
            duration: Tick(5 * TICKS_PER_SECOND),
            frame_rate: FrameRate::FPS_29_97,
            has_aegraphic: false,
            controls: Vec::new(),
        }
    }
}

impl MogrtDefinition {
    /// Visit every non-group control recursively in definition order.
    pub fn flat_controls(&self) -> Vec<&MogrtControl> {
        let mut out = Vec::new();
        collect_flat(&self.controls, &mut out, 0);
        out
    }

    /// Find a control by exact `id`, else case-insensitive `id`, else case-insensitive `ui_name`.
    pub fn find_control(&self, key: &str) -> Option<&MogrtControl> {
        let flat = self.flat_controls();
        flat.iter()
            .copied()
            .find(|c| c.id == key)
            .or_else(|| flat.iter().copied().find(|c| c.id.eq_ignore_ascii_case(key)))
            .or_else(|| flat.iter().copied().find(|c| c.ui_name.eq_ignore_ascii_case(key)))
    }
}

fn collect_flat<'a>(controls: &'a [MogrtControl], out: &mut Vec<&'a MogrtControl>, depth: usize) {
    if depth > MAX_CONTROL_DEPTH {
        return;
    }
    for c in controls {
        match &c.kind {
            MogrtControlKind::Group { children } => collect_flat(children, out, depth + 1),
            _ => out.push(c),
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Bounded clean-room ZIP archive reader & writer
// ------------------------------------------------------------------------------------------------

/// One entry header parsed from the ZIP Central Directory.
#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub comp_size: u32,
    pub uncomp_size: u32,
    pub local_header_offset: u32,
}

fn read_u16_le(bytes: &[u8], off: usize) -> Option<u16> {
    let arr: [u8; 2] = bytes.get(off..off.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(arr))
}

fn read_u32_le(bytes: &[u8], off: usize) -> Option<u32> {
    let arr: [u8; 4] = bytes.get(off..off.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
}

/// Locate the End of Central Directory record (`0x06054b50`) near the end of `bytes`.
fn find_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 22 {
        return None;
    }
    let min_pos = bytes.len().saturating_sub(22 + 65_535);
    let mut pos = bytes.len() - 22;
    loop {
        if read_u32_le(bytes, pos) == Some(0x0605_4b50) {
            let comment_len = usize::from(read_u16_le(bytes, pos + 20)?);
            if pos.checked_add(22)?.checked_add(comment_len) == Some(bytes.len()) {
                return Some(pos);
            }
        }
        if pos == min_pos {
            return None;
        }
        pos -= 1;
    }
}

/// Parse the Central Directory of a ZIP archive without decompressing any entries.
pub fn list_zip_entries(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    let eocd = find_eocd(bytes).ok_or_else(|| MediaError::Decode("invalid .mogrt archive: missing ZIP end-of-central-directory".into()))?;
    let total_entries = usize::from(read_u16_le(bytes, eocd + 10).ok_or_else(|| MediaError::Decode("truncated ZIP EOCD".into()))?);
    if total_entries > MAX_ZIP_ENTRIES {
        return Err(MediaError::Unsupported(format!(".mogrt archive has {total_entries} entries (limit {MAX_ZIP_ENTRIES})")));
    }
    let cd_size = read_u32_le(bytes, eocd + 12).ok_or_else(|| MediaError::Decode("truncated ZIP EOCD".into()))? as usize;
    let cd_offset = read_u32_le(bytes, eocd + 16).ok_or_else(|| MediaError::Decode("truncated ZIP EOCD".into()))? as usize;
    let cd_end = cd_offset.checked_add(cd_size).ok_or_else(|| MediaError::Decode("ZIP central directory offset overflow".into()))?;
    if cd_end > bytes.len() {
        return Err(MediaError::Decode("ZIP central directory extends past end of file".into()));
    }

    let mut entries = Vec::with_capacity(total_entries.min(256));
    let mut pos = cd_offset;
    for _ in 0..total_entries {
        if pos.checked_add(46).is_none_or(|e| e > cd_end) {
            return Err(MediaError::Decode("truncated ZIP central directory entry".into()));
        }
        if read_u32_le(bytes, pos) != Some(0x0201_4b50) {
            return Err(MediaError::Decode("invalid ZIP central directory signature".into()));
        }
        let method = read_u16_le(bytes, pos + 10).ok_or_else(|| MediaError::Decode("truncated ZIP entry".into()))?;
        let comp_size = read_u32_le(bytes, pos + 20).ok_or_else(|| MediaError::Decode("truncated ZIP entry".into()))?;
        let uncomp_size = read_u32_le(bytes, pos + 24).ok_or_else(|| MediaError::Decode("truncated ZIP entry".into()))?;
        let name_len = usize::from(read_u16_le(bytes, pos + 28).ok_or_else(|| MediaError::Decode("truncated ZIP entry".into()))?);
        let extra_len = usize::from(read_u16_le(bytes, pos + 30).ok_or_else(|| MediaError::Decode("truncated ZIP entry".into()))?);
        let comment_len = usize::from(read_u16_le(bytes, pos + 32).ok_or_else(|| MediaError::Decode("truncated ZIP entry".into()))?);
        let local_header_offset = read_u32_le(bytes, pos + 42).ok_or_else(|| MediaError::Decode("truncated ZIP entry".into()))?;

        let name_start = pos + 46;
        let name_end = name_start.checked_add(name_len).ok_or_else(|| MediaError::Decode("ZIP entry name overflow".into()))?;
        let next_pos = name_end
            .checked_add(extra_len)
            .and_then(|v| v.checked_add(comment_len))
            .ok_or_else(|| MediaError::Decode("ZIP entry length overflow".into()))?;
        if next_pos > cd_end {
            return Err(MediaError::Decode("ZIP central directory entry exceeds directory bounds".into()));
        }
        let name_bytes = bytes.get(name_start..name_end).ok_or_else(|| MediaError::Decode("ZIP entry name out of bounds".into()))?;
        let name = String::from_utf8_lossy(name_bytes).replace('\\', "/");
        entries.push(ZipEntry { name, method, comp_size, uncomp_size, local_header_offset });
        pos = next_pos;
    }
    Ok(entries)
}

/// Decompress a single [`ZipEntry`] from `bytes` with a strict `max_bytes` upper bound.
pub fn extract_zip_entry(bytes: &[u8], entry: &ZipEntry, max_bytes: usize) -> Result<Vec<u8>> {
    if entry.comp_size == u32::MAX || entry.uncomp_size == u32::MAX {
        return Err(MediaError::Unsupported(format!("{}: ZIP64 entries are not supported", entry.name)));
    }
    if entry.uncomp_size as usize > max_bytes {
        return Err(MediaError::Unsupported(format!("{}: uncompressed size {} exceeds {} byte limit", entry.name, entry.uncomp_size, max_bytes)));
    }
    let off = entry.local_header_offset as usize;
    if read_u32_le(bytes, off) != Some(0x0403_4b50) {
        return Err(MediaError::Decode(format!("{}: invalid ZIP local file header", entry.name)));
    }
    let name_len = usize::from(read_u16_le(bytes, off + 26).ok_or_else(|| MediaError::Decode("truncated ZIP local header".into()))?);
    let extra_len = usize::from(read_u16_le(bytes, off + 28).ok_or_else(|| MediaError::Decode("truncated ZIP local header".into()))?);
    let data_start = off
        .checked_add(30)
        .and_then(|v| v.checked_add(name_len))
        .and_then(|v| v.checked_add(extra_len))
        .ok_or_else(|| MediaError::Decode("ZIP local header offset overflow".into()))?;
    let data_end = data_start.checked_add(entry.comp_size as usize).ok_or_else(|| MediaError::Decode("ZIP compressed data overflow".into()))?;
    let payload = bytes.get(data_start..data_end).ok_or_else(|| MediaError::Decode(format!("{}: compressed data extends past EOF", entry.name)))?;

    match entry.method {
        0 => {
            if payload.len() > max_bytes {
                return Err(MediaError::Unsupported(format!("{}: entry exceeds {} byte limit", entry.name, max_bytes)));
            }
            Ok(payload.to_vec())
        }
        8 => {
            let mut decoder = flate2::read::DeflateDecoder::new(payload).take(max_bytes as u64 + 1);
            let mut out = Vec::with_capacity((entry.uncomp_size as usize).min(max_bytes).min(1024 * 1024));
            decoder.read_to_end(&mut out).map_err(|e| MediaError::Decode(format!("{}: deflate error: {e}", entry.name)))?;
            if out.len() > max_bytes {
                return Err(MediaError::Unsupported(format!("{}: decompressed data exceeds {} byte limit", entry.name, max_bytes)));
            }
            Ok(out)
        }
        other => Err(MediaError::Unsupported(format!("{}: unsupported ZIP compression method {other}", entry.name))),
    }
}

/// Build a valid ZIP archive in memory (Stored or Deflate). Useful for tests and fixtures.
pub fn build_zip(entries: &[(&str, &[u8])], compress: bool) -> Vec<u8> {
    use std::io::Write;
    let mut out = Vec::new();
    let mut central: Vec<(Vec<u8>, u16, u32, u32, u32, u32)> = Vec::with_capacity(entries.len());

    for &(name, data) in entries {
        let name_bytes = name.as_bytes();
        let crc = crc32(data);
        let (method, payload): (u16, Vec<u8>) = if compress {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            if enc.write_all(data).is_ok()
                && let Ok(compressed) = enc.finish()
            {
                (8, compressed)
            } else {
                (0, data.to_vec())
            }
        } else {
            (0, data.to_vec())
        };
        let local_off = out.len() as u32;
        let comp_size = payload.len() as u32;
        let uncomp_size = data.len() as u32;

        // Local file header
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&comp_size.to_le_bytes());
        out.extend_from_slice(&uncomp_size.to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&payload);

        central.push((name_bytes.to_vec(), method, crc, comp_size, uncomp_size, local_off));
    }

    let cd_offset = out.len() as u32;
    for (name_bytes, method, crc, comp_size, uncomp_size, local_off) in &central {
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&comp_size.to_le_bytes());
        out.extend_from_slice(&uncomp_size.to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out.extend_from_slice(&0u16.to_le_bytes()); // disk number
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&local_off.to_le_bytes());
        out.extend_from_slice(name_bytes);
    }
    let cd_size = (out.len() as u32).saturating_sub(cd_offset);

    // End of central directory
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number
    out.extend_from_slice(&0u16.to_le_bytes()); // start disk
    out.extend_from_slice(&(central.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if (crc & 1) != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

// ------------------------------------------------------------------------------------------------
// Archive extraction & definition.json parsing
// ------------------------------------------------------------------------------------------------

/// Parsed contents of a `.mogrt` ZIP archive.
#[derive(Clone, Debug)]
pub struct MogrtArchive {
    pub definition: MogrtDefinition,
    pub thumb_png: Option<Vec<u8>>,
    pub has_aegraphic: bool,
    pub entry_names: Vec<String>,
}

/// Whether `name` and `bytes` look like a `.mogrt` ZIP archive.
pub fn sniff(name: &str, bytes: &[u8]) -> bool {
    name.to_ascii_lowercase().ends_with(".mogrt") && bytes.starts_with(b"PK\x03\x04")
}

fn find_entry_ci<'a>(entries: &'a [ZipEntry], target: &str) -> Option<&'a ZipEntry> {
    let lower = target.to_ascii_lowercase();
    entries.iter().find(|e| e.name.trim_start_matches("./").eq_ignore_ascii_case(&lower)).or_else(|| {
        entries.iter().find(|e| {
            let file = e.name.rsplit('/').next().unwrap_or(&e.name);
            file.eq_ignore_ascii_case(&lower)
        })
    })
}

/// Parse a `.mogrt` archive from raw ZIP bytes.
pub fn parse_archive(name: &str, bytes: &[u8]) -> Result<MogrtArchive> {
    let entries = list_zip_entries(bytes)?;
    let def_entry = find_entry_ci(&entries, "definition.json").ok_or_else(|| MediaError::Decode(format!("{name}: .mogrt archive is missing definition.json")))?;
    let def_bytes = extract_zip_entry(bytes, def_entry, MAX_DEFINITION_BYTES)?;

    let has_aegraphic = entries.iter().any(|e| {
        let low = e.name.to_ascii_lowercase();
        low == "project.aegraphic" || low.ends_with("/project.aegraphic") || low.ends_with(".aegraphic")
    });

    let fallback_name = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let mut definition = parse_definition_json(&def_bytes, fallback_name)?;
    definition.has_aegraphic = has_aegraphic;

    let thumb_png = ["thumb.png", "thumbnail.png", "preview.png"]
        .iter()
        .find_map(|candidate| find_entry_ci(&entries, candidate))
        .and_then(|e| extract_zip_entry(bytes, e, MAX_THUMB_BYTES).ok());

    let entry_names = entries.into_iter().map(|e| e.name).collect();
    Ok(MogrtArchive { definition, thumb_png, has_aegraphic, entry_names })
}

/// Extract every safe, bounded entry from a `.mogrt` ZIP archive into memory (used by the
/// optional After Effects `aerender` bridge).
pub fn extract_all_safe(bytes: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    let entries = list_zip_entries(bytes)?;
    let mut out = HashMap::new();
    let mut total_bytes: usize = 0;
    const MAX_TOTAL: usize = 256 * 1024 * 1024;
    for entry in &entries {
        if entry.name.ends_with('/') || entry.name.is_empty() {
            continue;
        }
        let clean = sanitize_zip_path(&entry.name);
        if clean.is_empty() {
            continue;
        }
        let data = extract_zip_entry(bytes, entry, MAX_ASSET_BYTES)?;
        total_bytes = total_bytes.saturating_add(data.len());
        if total_bytes > MAX_TOTAL {
            return Err(MediaError::Unsupported("extracted .mogrt archive exceeds 256 MiB safety limit".into()));
        }
        out.insert(clean, data);
    }
    Ok(out)
}

/// Strip leading slashes and parent-directory segments (`..`) from a ZIP entry path.
fn sanitize_zip_path(raw: &str) -> String {
    let parts: Vec<&str> = raw.split('/').filter(|seg| !seg.is_empty() && *seg != "." && *seg != ".." && !seg.contains(':')).collect();
    parts.join("/")
}

/// Parse `definition.json` bytes into a [`MogrtDefinition`].
pub fn parse_definition_json(bytes: &[u8], fallback_name: &str) -> Result<MogrtDefinition> {
    // Strip optional UTF-8 BOM.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let root: Value = serde_json::from_slice(bytes).map_err(|e| MediaError::Decode(format!("definition.json parse error: {e}")))?;
    let obj = root.as_object().ok_or_else(|| MediaError::Decode("definition.json root must be a JSON object".into()))?;

    let capsule_name = obj
        .get("capsuleName")
        .and_then(extract_localized_str)
        .or_else(|| obj.get("name").and_then(extract_localized_str))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| if fallback_name.is_empty() { "Motion Graphics Template".into() } else { fallback_name.to_string() });

    let capsule_id = obj.get("capsuleID").or_else(|| obj.get("capsuleId")).and_then(Value::as_str).unwrap_or_default().to_string();

    let (width, height) = parse_frame_size(&root);
    let frame_rate = parse_frame_rate(&root);
    let duration = parse_duration(&root, frame_rate);

    let mut counter = 0usize;
    let controls = obj.get("clientControls").and_then(Value::as_array).map(|arr| parse_control_list(arr, None, 0, &mut counter)).unwrap_or_default();

    Ok(MogrtDefinition { capsule_name, capsule_id, width, height, duration, frame_rate, has_aegraphic: false, controls })
}

fn source_info_locale(root: &Value) -> Option<&Value> {
    let map = root.get("sourceInfoLocalized")?.as_object()?;
    map.get("en_US").or_else(|| map.values().next())
}

fn parse_frame_size(root: &Value) -> (u32, u32) {
    let from_val = |v: &Value| -> Option<(u32, u32)> {
        if let Some(arr) = v.as_array()
            && arr.len() >= 2
        {
            let w = arr.first()?.as_f64()?;
            let h = arr.get(1)?.as_f64()?;
            return clamp_size(w, h);
        }
        if let Some(obj) = v.as_object() {
            let w = obj.get("x").or_else(|| obj.get("width")).or_else(|| obj.get("w")).and_then(Value::as_f64)?;
            let h = obj.get("y").or_else(|| obj.get("height")).or_else(|| obj.get("h")).and_then(Value::as_f64)?;
            return clamp_size(w, h);
        }
        None
    };

    if let Some(loc) = source_info_locale(root)
        && let Some(sz) = loc.get("framesize").or_else(|| loc.get("frameSize")).and_then(from_val)
    {
        return sz;
    }
    if let Some(sz) = root.get("framesize").or_else(|| root.get("frameSize")).and_then(from_val) {
        return sz;
    }
    if let (Some(w), Some(h)) = (root.get("width").and_then(Value::as_f64), root.get("height").and_then(Value::as_f64))
        && let Some(sz) = clamp_size(w, h)
    {
        return sz;
    }
    (1920, 1080)
}

fn clamp_size(w: f64, h: f64) -> Option<(u32, u32)> {
    if !w.is_finite() || !h.is_finite() || w < 1.0 || h < 1.0 {
        return None;
    }
    let wi = (w.round() as u32).clamp(1, MAX_TEMPLATE_SIDE);
    let hi = (h.round() as u32).clamp(1, MAX_TEMPLATE_SIDE);
    Some((wi, hi))
}

fn parse_frame_rate(root: &Value) -> FrameRate {
    let parse_rate_val = |v: &Value| -> Option<FrameRate> {
        if let Some(fps) = v.as_f64()
            && fps.is_finite()
            && (1.0..=240.0).contains(&fps)
        {
            return Some(FrameRate::from_f64(fps));
        }
        if let Some(obj) = v.as_object()
            && let (Some(num), Some(den)) = (obj.get("num").and_then(Value::as_u64), obj.get("den").and_then(Value::as_u64))
            && num > 0
            && den > 0
        {
            let fps = num as f64 / den as f64;
            if fps.is_finite() && (1.0..=240.0).contains(&fps) {
                return Some(FrameRate::from_f64(fps));
            }
        }
        None
    };

    for candidate in [
        root.get("framerate"),
        root.get("frameRate"),
        root.get("fps"),
        source_info_locale(root).and_then(|l| l.get("framerate")),
        source_info_locale(root).and_then(|l| l.get("frameRate")),
    ] {
        if let Some(rate) = candidate.and_then(parse_rate_val) {
            return rate;
        }
    }
    FrameRate::FPS_29_97
}

fn parse_duration(root: &Value, rate: FrameRate) -> Tick {
    let raw = root
        .get("duration")
        .or_else(|| root.get("capsuleDuration"))
        .or_else(|| root.get("baseDuration"))
        .or_else(|| source_info_locale(root).and_then(|l| l.get("duration")))
        .and_then(Value::as_f64);

    let secs = match raw {
        Some(v) if v.is_finite() && v > 0.0 => {
            // Standard AE/Premiere `.mogrt` stores seconds (or Premiere ticks if > 1e8).
            let s = if v > 1e8 { v / TICKS_PER_SECOND as f64 } else { v };
            s.clamp(0.1, MAX_DURATION_SECS)
        }
        _ => DEFAULT_DURATION_SECS,
    };
    rate.snap_nearest(Tick::from_seconds_f64(secs)).max(rate.frame_duration())
}

/// Extract an English / default string from either a plain JSON string or a Premiere `strDB`
/// localized object (`{"strDB": [{"localeString": "en_US", "str": "..."}]}`).
fn extract_localized_str(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    let obj = v.as_object()?;
    if let Some(s) = obj.get("str").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    if let Some(arr) = obj.get("strDB").and_then(Value::as_array) {
        let en = arr
            .iter()
            .find(|item| item.get("localeString").and_then(Value::as_str).is_some_and(|l| l.eq_ignore_ascii_case("en_US")))
            .or_else(|| arr.first());
        if let Some(s) = en.and_then(|item| item.get("str")).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

fn parse_control_list(arr: &[Value], inherited_group: Option<&str>, depth: usize, counter: &mut usize) -> Vec<MogrtControl> {
    if depth > MAX_CONTROL_DEPTH {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut current_flat_group: Option<String> = inherited_group.map(str::to_string);

    for item in arr {
        if *counter >= MAX_CONTROLS {
            break;
        }
        let Some(obj) = item.as_object() else { continue };
        let ctype = obj.get("type").and_then(Value::as_u64).unwrap_or(8) as u32;
        *counter += 1;
        let idx = *counter;

        let ui_name = obj
            .get("uiName")
            .and_then(extract_localized_str)
            .or_else(|| obj.get("name").and_then(extract_localized_str))
            .unwrap_or_else(|| format!("Control {idx}"));

        let id = obj
            .get("id")
            .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("ctl_{idx}"));

        if ctype == 10 {
            // Group / collapsible header: may carry nested `clientControls` OR start a flat group.
            let children = obj
                .get("clientControls")
                .or_else(|| obj.get("children"))
                .and_then(Value::as_array)
                .map(|sub| parse_control_list(sub, Some(&ui_name), depth + 1, counter))
                .unwrap_or_default();
            if children.is_empty() {
                current_flat_group = Some(ui_name.clone());
            }
            out.push(MogrtControl { id, ui_name, control_type: 10, group: inherited_group.map(str::to_string), kind: MogrtControlKind::Group { children } });
            continue;
        }

        let kind = parse_control_kind(ctype, obj);
        out.push(MogrtControl { id, ui_name, control_type: ctype, group: current_flat_group.clone(), kind });
    }
    out
}

fn parse_control_kind(ctype: u32, obj: &serde_json::Map<String, Value>) -> MogrtControlKind {
    match ctype {
        6 => parse_text_control(obj),
        13 => parse_dropdown_control(obj),
        2 => parse_slider_control(obj),
        1 | 3 => {
            let val = obj.get("value");
            let b = val
                .and_then(Value::as_bool)
                .or_else(|| val.and_then(Value::as_f64).map(|n| n != 0.0))
                .or_else(|| val.and_then(|v| v.get("value")).and_then(Value::as_bool))
                .unwrap_or(false);
            MogrtControlKind::Checkbox { default_value: b }
        }
        4 => MogrtControlKind::Color { default_rgba: parse_color_value(obj.get("value")) },
        5 => MogrtControlKind::Point { default_point: parse_point_value(obj.get("value")) },
        _ => {
            let text = obj.get("value").and_then(extract_localized_str).or_else(|| obj.get("text").and_then(extract_localized_str)).unwrap_or_default();
            let hidden_by_default = is_hidden_info_string(&text);
            MogrtControlKind::StringInfo { text, hidden_by_default }
        }
    }
}

fn parse_text_control(obj: &serde_json::Map<String, Value>) -> MogrtControlKind {
    let val = obj.get("value");
    let font_obj = obj
        .get("fontEditValue")
        .or_else(|| val.and_then(|v| v.get("fontEditValue")))
        .or_else(|| val.and_then(|v| v.get("fontTextRunLength")).and(val));

    let text = val
        .and_then(|v| v.get("textEditValue").and_then(Value::as_str).map(str::to_string))
        .or_else(|| font_obj.and_then(|f| f.get("textEditValue").and_then(Value::as_str).map(str::to_string)))
        .or_else(|| val.and_then(extract_localized_str))
        .or_else(|| obj.get("text").and_then(extract_localized_str))
        .unwrap_or_default();

    let mut font = MogrtFontValue::default();
    if let Some(f) = font_obj.and_then(Value::as_object) {
        if let Some(fam) = f.get("fontFamily").or_else(|| f.get("fontEditValue")).or_else(|| f.get("font")).and_then(Value::as_str) {
            font.font_family = fam.to_string();
        } else if let Some(fam) = f.get("fontEditValue").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str) {
            font.font_family = fam.to_string();
        }
        if let Some(st) = f.get("fontStyle").or_else(|| f.get("style")).and_then(Value::as_str) {
            font.font_style = st.to_string();
        }
        if let Some(sz) = f
            .get("fontSizeEditValue")
            .or_else(|| f.get("fontSize"))
            .or_else(|| f.get("size"))
            .and_then(|v| v.as_f64().or_else(|| v.as_array().and_then(|a| a.first()).and_then(Value::as_f64)))
            && sz.is_finite()
            && sz > 0.0
        {
            font.font_size = Some((sz as f32).clamp(4.0, 1000.0));
        }
        font.faux_bold = f.get("fontFauxStyleBold").or_else(|| f.get("fauxBold")).and_then(Value::as_bool).unwrap_or(false);
        font.faux_italic = f.get("fontFauxStyleItalic").or_else(|| f.get("fauxItalic")).and_then(Value::as_bool).unwrap_or(false);
        font.all_caps = f.get("fontAllCaps").or_else(|| f.get("allCaps")).and_then(Value::as_bool).unwrap_or(false);
        font.small_caps = f.get("fontSmallCaps").or_else(|| f.get("smallCaps")).and_then(Value::as_bool).unwrap_or(false);
        if let Some(tr) = f.get("tracking").and_then(Value::as_f64)
            && tr.is_finite()
        {
            font.tracking = Some((tr as f32).clamp(-1000.0, 2000.0));
        }
        if let Some(ld) = f.get("leading").and_then(Value::as_f64)
            && ld.is_finite()
        {
            font.leading = Some((ld as f32).clamp(-500.0, 1000.0));
        }
    }
    MogrtControlKind::Text { default_text: text, font }
}

fn parse_dropdown_control(obj: &serde_json::Map<String, Value>) -> MogrtControlKind {
    let raw_items = obj
        .get("menucontent")
        .or_else(|| obj.get("menuContent"))
        .or_else(|| obj.get("options"))
        .or_else(|| obj.get("items"))
        .and_then(Value::as_array);

    let mut options = Vec::new();
    if let Some(arr) = raw_items {
        for (i, entry) in arr.iter().take(256).enumerate() {
            if let Some(s) = entry.as_str() {
                options.push(MogrtMenuOption { name: s.to_string(), value: i as i64 });
            } else if let Some(m) = entry.as_object() {
                let name = m
                    .get("name")
                    .and_then(extract_localized_str)
                    .or_else(|| m.get("label").and_then(extract_localized_str))
                    .or_else(|| m.get("uiName").and_then(extract_localized_str))
                    .or_else(|| extract_localized_str(entry))
                    .unwrap_or_else(|| format!("Option {}", i + 1));
                let value = m.get("value").and_then(Value::as_i64).unwrap_or(i as i64);
                options.push(MogrtMenuOption { name, value });
            }
        }
    }

    let raw_default = obj
        .get("value")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)).or_else(|| v.get("value").and_then(Value::as_i64)))
        .unwrap_or(0);

    let default_index = if options.is_empty() {
        0
    } else if let Some(pos) = options.iter().position(|o| o.value == raw_default) {
        pos
    } else if let Ok(i) = usize::try_from(raw_default)
        && i < options.len()
    {
        i
    } else if let Some(prev) = raw_default.checked_sub(1)
        && let Ok(i) = usize::try_from(prev)
        && i < options.len()
    {
        // AE dropdowns are often 1-based.
        i
    } else {
        0
    };

    MogrtControlKind::Dropdown { options, default_index }
}

fn parse_slider_control(obj: &serde_json::Map<String, Value>) -> MogrtControlKind {
    let val = obj.get("value").and_then(|v| v.as_f64().or_else(|| v.get("value").and_then(Value::as_f64))).filter(|v| v.is_finite()).unwrap_or(0.0);

    let min = obj
        .get("min")
        .or_else(|| obj.get("uiRangeMin"))
        .or_else(|| obj.get("sliderMin"))
        .or_else(|| obj.get("valueRange").and_then(|r| r.get("min")))
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite());

    let max = obj
        .get("max")
        .or_else(|| obj.get("uiRangeMax"))
        .or_else(|| obj.get("sliderMax"))
        .or_else(|| obj.get("valueRange").and_then(|r| r.get("max")))
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite());

    // Normalize inverted bounds so min <= max always holds.
    let (min, max) = match (min, max) {
        (Some(a), Some(b)) if a > b => (Some(b), Some(a)),
        pair => pair,
    };

    MogrtControlKind::Slider { default_value: val, min, max }
}

fn parse_color_value(val: Option<&Value>) -> [f32; 4] {
    let Some(v) = val else { return [1.0, 1.0, 1.0, 1.0] };
    if let Some(arr) = v.as_array()
        && arr.len() >= 3
    {
        let r = arr.first().and_then(Value::as_f64).unwrap_or(1.0);
        let g = arr.get(1).and_then(Value::as_f64).unwrap_or(1.0);
        let b = arr.get(2).and_then(Value::as_f64).unwrap_or(1.0);
        let a = arr.get(3).and_then(Value::as_f64).unwrap_or(1.0);
        return normalize_rgba(r, g, b, a);
    }
    if let Some(obj) = v.as_object() {
        if let Some(inner) = obj.get("color").or_else(|| obj.get("value")) {
            return parse_color_value(Some(inner));
        }
        let r = obj.get("r").or_else(|| obj.get("red")).and_then(Value::as_f64).unwrap_or(1.0);
        let g = obj.get("g").or_else(|| obj.get("green")).and_then(Value::as_f64).unwrap_or(1.0);
        let b = obj.get("b").or_else(|| obj.get("blue")).and_then(Value::as_f64).unwrap_or(1.0);
        let a = obj.get("a").or_else(|| obj.get("alpha")).and_then(Value::as_f64).unwrap_or(1.0);
        return normalize_rgba(r, g, b, a);
    }
    if let Some(hex) = v.as_str()
        && let Some(c) = parse_hex_rgba(hex)
    {
        return c;
    }
    [1.0, 1.0, 1.0, 1.0]
}

fn normalize_rgba(r: f64, g: f64, b: f64, a: f64) -> [f32; 4] {
    let clean = |x: f64, fallback: f64| if x.is_finite() { x } else { fallback };
    let (r, g, b, a) = (clean(r, 1.0), clean(g, 1.0), clean(b, 1.0), clean(a, 1.0));
    let scale = if r > 1.0 || g > 1.0 || b > 1.0 || a > 1.0 { 255.0 } else { 1.0 };
    [(r / scale).clamp(0.0, 1.0) as f32, (g / scale).clamp(0.0, 1.0) as f32, (b / scale).clamp(0.0, 1.0) as f32, (a / scale).clamp(0.0, 1.0) as f32]
}

fn parse_hex_rgba(s: &str) -> Option<[f32; 4]> {
    let hex = s.trim().strip_prefix('#')?;
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok().map(|b| f32::from(b) / 255.0);
    let r = byte(0)?;
    let g = byte(2)?;
    let b = byte(4)?;
    let a = if hex.len() == 8 { byte(6)? } else { 1.0 };
    Some([r, g, b, a])
}

fn parse_point_value(val: Option<&Value>) -> [f64; 2] {
    let Some(v) = val else { return [0.0, 0.0] };
    if let Some(arr) = v.as_array()
        && arr.len() >= 2
    {
        let x = arr.first().and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0);
        let y = arr.get(1).and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0);
        return [x, y];
    }
    if let Some(obj) = v.as_object() {
        if let Some(inner) = obj.get("value") {
            return parse_point_value(Some(inner));
        }
        let x = obj.get("x").and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0);
        let y = obj.get("y").and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0);
        return [x, y];
    }
    [0.0, 0.0]
}

/// Heuristic to detect empty or promotional/credit read-only `StringInfo` (`type: 8`) controls
/// commonly embedded in third-party `.mogrt` packs so the Inspector stays clean.
pub fn is_hidden_info_string(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    let low = trimmed.to_ascii_lowercase();
    low.contains("http://")
        || low.contains("https://")
        || low.contains("www.")
        || low.contains("envato")
        || low.contains("motionarray")
        || low.contains("videohive")
        || low.contains("subscribe")
        || low.contains("copyright")
        || low.contains("©")
        || low.starts_with("created by")
        || low.starts_with("made by")
        || low.starts_with("designed by")
}

// ------------------------------------------------------------------------------------------------
// MediaSource implementation
// ------------------------------------------------------------------------------------------------

/// A `.mogrt` archive exposed as a [`MediaSource`].
///
/// Serves `thumb.png` (or a clean transparent frame matching the template's canvas size) for
/// Media Browser, Project panel, Source Monitor, and timeline clip thumbnails, while exposing
/// the parsed [`MogrtDefinition`] for project storage and dynamic rendering.
pub struct MogrtSource {
    info: MediaInfo,
    definition: MogrtDefinition,
    thumb: Arc<VideoFrame>,
}

impl MogrtSource {
    /// Parse a `.mogrt` file from raw bytes into a [`MogrtSource`].
    pub fn parse(name: &str, bytes: &[u8]) -> Result<Self> {
        let archive = parse_archive(name, bytes)?;
        let def = archive.definition;

        let thumb = match archive.thumb_png.as_deref().and_then(|png| crate::still::decode_raster("thumb.png", png).ok()) {
            Some(r) => Arc::new(VideoFrame::rgba8(r.width, r.height, r.rgba)),
            None => {
                // Lightweight transparent fallback thumbnail preserving aspect ratio.
                let (tw, th) = fallback_thumb_size(def.width, def.height);
                Arc::new(VideoFrame::rgba8(tw, th, vec![0u8; (tw * th * 4) as usize]))
            }
        };

        let info = MediaInfo {
            name: def.capsule_name.clone(),
            kind: MediaKind::Movie,
            duration: def.duration,
            video: Some(VideoStreamInfo {
                width: def.width,
                height: def.height,
                frame_rate: def.frame_rate,
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
            file_size: Some(bytes.len() as u64),
        };
        Ok(Self { info, definition: def, thumb })
    }

    pub fn definition(&self) -> &MogrtDefinition {
        &self.definition
    }
}

fn fallback_thumb_size(w: u32, h: u32) -> (u32, u32) {
    let w = w.max(1);
    let h = h.max(1);
    let max_side = 320u32;
    if w >= h {
        let th = ((u64::from(h) * u64::from(max_side)) / u64::from(w)).max(1) as u32;
        (max_side, th)
    } else {
        let tw = ((u64::from(w) * u64::from(max_side)) / u64::from(h)).max(1) as u32;
        (tw, max_side)
    }
}

impl MediaSource for MogrtSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn video_frame(&self, req: FrameRequest) -> Result<Arc<VideoFrame>> {
        // Clone metadata with requested pts while sharing pixel storage cheaply via new Arc.
        let mut frame = (*self.thumb).clone();
        frame.pts = req.time;
        Ok(Arc::new(frame))
    }

    fn audio(&self, _start: i64, frames: usize, sample_rate: u32) -> Result<AudioBuffer> {
        validate_audio_buffer_shape(2, frames)?;
        Ok(AudioBuffer::silence(sample_rate, 2, frames))
    }

    fn mogrt_definition(&self) -> Option<&MogrtDefinition> {
        Some(&self.definition)
    }
}

// ------------------------------------------------------------------------------------------------
// Unit tests
// ------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        let mut buf = Vec::new();
        let img = image::RgbaImage::from_pixel(4, 3, image::Rgba([220, 80, 40, 255]));
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buf
    }

    fn sample_definition_json() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "capsuleName": "Modern Lower Third",
            "capsuleID": "cap-001",
            "sourceInfoLocalized": {
                "en_US": {
                    "framesize": [1920, 1080]
                }
            },
            "duration": 6.0,
            "framerate": 30.0,
            "clientControls": [
                {
                    "id": "grp_text",
                    "type": 10,
                    "uiName": { "strDB": [{ "localeString": "en_US", "str": "Text Settings" }] },
                    "clientControls": [
                        {
                            "id": "src_text",
                            "type": 6,
                            "uiName": { "strDB": [{ "localeString": "en_US", "str": "Source Text" }] },
                            "value": {
                                "textEditValue": "Hello CADR",
                                "fontEditValue": {
                                    "fontFamily": "Inter",
                                    "fontStyle": "Bold",
                                    "fontSizeEditValue": 84.0,
                                    "fontAllCaps": true
                                }
                            }
                        },
                        {
                            "id": "txt_color",
                            "type": 4,
                            "uiName": "Text Color",
                            "value": [0.95, 0.25, 0.1, 1.0]
                        },
                        {
                            "id": "txt_size",
                            "type": 2,
                            "uiName": "Text Size",
                            "value": 84.0,
                            "min": 12.0,
                            "max": 300.0
                        },
                        {
                            "id": "txt_pos",
                            "type": 5,
                            "uiName": "Text Position",
                            "value": { "x": 960.0, "y": 540.0 }
                        }
                    ]
                },
                {
                    "id": "anim_preset",
                    "type": 13,
                    "uiName": "Animation Preset",
                    "value": 1,
                    "menucontent": [
                        { "name": "None", "value": 0 },
                        { "name": "Slide Up", "value": 1 },
                        { "name": "Smooth Zoom", "value": 2 },
                        { "name": "Smooth Tracking", "value": 3 },
                        { "name": "Smooth Blur", "value": 4 },
                        { "name": "Smooth Fade", "value": 5 }
                    ]
                },
                {
                    "id": "promo_banner",
                    "type": 8,
                    "uiName": "Credits",
                    "value": "Created by Example - https://example.com"
                },
                {
                    "id": "tip_info",
                    "type": 8,
                    "uiName": "Usage Note",
                    "value": "Adjust speed slider for faster transitions"
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn parses_stored_and_deflated_mogrt_archives() {
        let def = sample_definition_json();
        let png = tiny_png();
        for compress in [false, true] {
            let zip = build_zip(&[("definition.json", &def), ("thumb.png", &png), ("project.aegraphic", b"ae-bytes")], compress);
            assert!(sniff("title.mogrt", &zip));
            let src = MogrtSource::parse("title.mogrt", &zip).unwrap();
            let d = src.definition();
            assert_eq!(d.capsule_name, "Modern Lower Third");
            assert_eq!((d.width, d.height), (1920, 1080));
            assert!(d.has_aegraphic);
            assert_eq!(src.info().container, "MOGRT");
            assert_eq!(src.info().duration, Tick(6 * TICKS_PER_SECOND));

            // Thumbnail frame decoded cleanly
            let frame = src.video_frame(FrameRequest::full(Tick::ZERO)).unwrap();
            assert_eq!((frame.width, frame.height), (4, 3));

            // Controls parsed accurately
            let flat = d.flat_controls();
            assert_eq!(flat.len(), 7);
            match &d.find_control("src_text").unwrap().kind {
                MogrtControlKind::Text { default_text, font } => {
                    assert_eq!(default_text, "Hello CADR");
                    assert_eq!(font.font_family, "Inter");
                    assert_eq!(font.font_size, Some(84.0));
                    assert!(font.all_caps);
                }
                other => panic!("expected text control, got {other:?}"),
            }
            match &d.find_control("anim_preset").unwrap().kind {
                MogrtControlKind::Dropdown { options, default_index } => {
                    assert_eq!(options.len(), 6);
                    assert_eq!(*default_index, 1);
                    assert_eq!(options[*default_index].name, "Slide Up");
                }
                other => panic!("expected dropdown control, got {other:?}"),
            }
            match &d.find_control("promo_banner").unwrap().kind {
                MogrtControlKind::StringInfo { hidden_by_default, .. } => assert!(*hidden_by_default),
                other => panic!("expected string info, got {other:?}"),
            }
            match &d.find_control("tip_info").unwrap().kind {
                MogrtControlKind::StringInfo { hidden_by_default, .. } => assert!(!*hidden_by_default),
                other => panic!("expected string info, got {other:?}"),
            }
        }
    }

    #[test]
    fn hostile_and_mutated_zip_inputs_never_panic() {
        let def = sample_definition_json();
        let valid = build_zip(&[("definition.json", &def)], true);
        // Truncations at every interesting boundary
        for cut in [0, 1, 4, 21, 22, valid.len() / 2, valid.len().saturating_sub(1)] {
            let _ = std::panic::catch_unwind(|| parse_archive("bad.mogrt", &valid[..cut])).unwrap();
        }
        // Bit flips across the archive
        for i in (0..valid.len()).step_by(7) {
            let mut mutated = valid.clone();
            mutated[i] ^= 0xFF;
            let _ = std::panic::catch_unwind(|| parse_archive("mut.mogrt", &mutated)).unwrap();
        }
        // Malformed JSON inside valid ZIP
        let bad_json = build_zip(&[("definition.json", b"{not valid json")], false);
        assert!(parse_archive("badjson.mogrt", &bad_json).is_err());
        // Missing definition.json
        let no_def = build_zip(&[("thumb.png", &tiny_png())], false);
        assert!(parse_archive("nodef.mogrt", &no_def).is_err());
    }
}
