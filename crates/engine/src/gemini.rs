//! Gemini BYOK captions, filler-word removal, editable SRT preview, and subtitle translation (`gemini.*`).
//!
//! Sends selected clip / sequence audio to the Gemini `v1beta` `generateContent` endpoint using
//! the user's own API key (passed via the `x-goog-api-key` HTTP header, never in the URL and never
//! logged). Reuses [`filmcraft_captions`] for SRT formatting/parsing and [`crate::captions::import`]
//! for importing the user's reviewed/edited SRT onto the active timeline as a native caption track.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use filmcraft_captions::{Cue, Document, Format, WriteOptions};
use filmcraft_project::ClipId;
use filmcraft_time::{TICKS_PER_SECOND, Tick};

use crate::commands::{CommandSpec, always, bad, bool_p, has_seq, str_p, u64_p};
use crate::secret_store::GEMINI_API_KEY;
use crate::silence::{ANALYSIS_SAMPLE_RATE, TimelineAudioSpan, extract_timeline_audio};
use crate::{EngineError, Result, Session};

/// Default recommended Gemini model ID (verified against current Gemini API docs & CADR CUT).
pub const DEFAULT_MODEL: &str = "gemini-3.8-flash";

/// Selectable Gemini models exposed in Settings and the Captions panel.
pub const GEMINI_MODELS: &[(&str, &str)] = &[
    ("gemini-3.8-flash", "Gemini 3.8 Flash (Recommended)"),
    ("gemini-3.6-flash", "Gemini 3.6 Flash"),
    ("gemini-3.5-flash-lite", "Gemini 3.5 Flash-Lite (Fast)"),
    ("gemini-2.5-flash-lite", "Gemini 2.5 Flash-Lite (Legacy - may require prior 2.5 usage on your Google account)"),
];

/// Supported subtitle translation target languages: `(prompt_name, iso_code, display_label)`.
pub const TRANSLATION_LANGUAGES: &[(&str, &str, &str)] = &[
    ("English", "en", "English"),
    ("Arabic", "ar", "Arabic"),
    ("French", "fr", "French"),
    ("Spanish", "es", "Spanish"),
    ("German", "de", "German"),
    ("Turkish", "tr", "Turkish"),
    ("Italian", "it", "Italian"),
    ("Portuguese", "pt", "Portuguese"),
    ("Russian", "ru", "Russian"),
    ("Hindi", "hi", "Hindi"),
    ("Urdu", "ur", "Urdu"),
    ("Indonesian", "id", "Indonesian"),
];

/// Default Arabic filler words (matching CADR CUT's built-in Arabic filler list).
pub const DEFAULT_ARABIC_FILLERS: &[&str] = &["يعني", "امم", "ام", "اه", "آه", "طيب", "خلاص", "يعنى", "ايه"];

/// Maximum inline base64 payload size accepted by Gemini `generateContent` (19 MiB safety cap).
const MAX_INLINE_BASE64_BYTES: usize = 19 * 1024 * 1024;

/// Maximum subtitle cues translated per Gemini request batch.
const TRANSLATION_BATCH_SIZE: usize = 60;

/// Maximum retry attempts for transient HTTP errors or schema fallback.
const MAX_ATTEMPTS: usize = 3;

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// HTTP transport abstraction for Gemini API calls so unit and UI tests run fully synthetic
/// without any live network calls or API spending.
pub trait GeminiTransport: Send + Sync {
    /// POST JSON `body` to `url` with `x-goog-api-key: api_key` header.
    /// Returns `(http_status_code, response_body_text)`.
    fn post_json(&self, url: &str, api_key: &str, body: &Value) -> std::result::Result<(u16, String), String>;
}

/// Default live HTTPS transport backed by `ureq`.
#[cfg(not(target_arch = "wasm32"))]
struct UreqTransport;

#[cfg(not(target_arch = "wasm32"))]
impl GeminiTransport for UreqTransport {
    fn post_json(&self, url: &str, api_key: &str, body: &Value) -> std::result::Result<(u16, String), String> {
        let payload = serde_json::to_string(body).map_err(|e| format!("failed to encode Gemini request: {e}"))?;
        let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
        let mut resp = agent
            .post(url)
            .header("Content-Type", "application/json")
            .header("x-goog-api-key", api_key)
            .send(payload.as_bytes())
            .map_err(|e| sanitize_secret(&format!("Gemini request failed: {e}"), api_key))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| sanitize_secret(&format!("failed to read Gemini response: {e}"), api_key))?;
        Ok((status, text))
    }
}

/// Strip any accidental occurrence of the API key from error strings before displaying or logging.
pub fn sanitize_secret(msg: &str, secret: &str) -> String {
    if secret.is_empty() {
        msg.to_string()
    } else {
        msg.replace(secret, "[REDACTED]")
    }
}

/// Build the official Gemini `v1beta` `generateContent` endpoint URL for `model`.
/// Never embeds the API key in the URL (authentication uses `x-goog-api-key` header).
pub fn endpoint_url(model: &str) -> Result<String> {
    let m = model.trim();
    if m.is_empty() || !m.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
        return Err(bad("gemini", format!("invalid Gemini model id `{m}`")));
    }
    Ok(format!("{GEMINI_API_BASE}/{m}:generateContent"))
}

/// A timestamped caption line returned by Gemini (times in seconds).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimedLine {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// A single timestamped spoken word returned by Gemini (times in seconds).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimedWord {
    pub word: String,
    pub start: f64,
    pub end: f64,
}

/// Parsed Gemini transcription response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptionResult {
    pub lines: Vec<TimedLine>,
    pub words: Vec<TimedWord>,
}

/// Encode mono f32 PCM samples (`[-1.0, 1.0]`) as a 16-bit signed PCM WAV byte buffer.
pub fn encode_wav_pcm16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format tag
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let q = (clamped * 32767.0).round() as i16;
        out.extend_from_slice(&q.to_le_bytes());
    }
    out
}

/// Normalize an Arabic or Latin word token for filler matching:
/// strips Arabic diacritics/tashkeel (`U+064B..=U+065F`, `U+0670`) and tatweel (`U+0640`),
/// normalizes alef variants (`إأآا` -> `ا`), taa marbuta (`ة` -> `ه`), alef maqsura (`ى` -> `ي`),
/// lowercases Latin characters, and drops surrounding punctuation.
pub fn normalize_filler_token(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for ch in word.chars() {
        match ch {
            '\u{064B}'..='\u{065F}' | '\u{0670}' | '\u{0640}' => {}
            'إ' | 'أ' | 'آ' | 'ٱ' => out.push('ا'),
            'ة' => out.push('ه'),
            'ى' => out.push('ي'),
            c if c.is_alphanumeric() => {
                for lc in c.to_lowercase() {
                    out.push(lc);
                }
            }
            _ => {}
        }
    }
    out
}

/// Build the combined normalized filler set from [`DEFAULT_ARABIC_FILLERS`] plus `custom_fillers`.
pub fn filler_set(custom_fillers: &[String]) -> Vec<String> {
    let mut set = Vec::new();
    for raw in DEFAULT_ARABIC_FILLERS.iter().copied().chain(custom_fillers.iter().map(String::as_str)) {
        let n = normalize_filler_token(raw);
        if !n.is_empty() && !set.contains(&n) {
            set.push(n);
        }
    }
    set
}

/// Check whether `word` matches any filler in `normalized_fillers`.
pub fn is_filler_word(word: &str, normalized_fillers: &[String]) -> bool {
    let n = normalize_filler_token(word);
    !n.is_empty() && normalized_fillers.iter().any(|f| f == &n)
}

/// Remove filler words from both `lines` and `words`, returning `(cleaned_lines, cleaned_words, filler_hits)`.
pub fn remove_fillers_from_captions(
    lines: &[TimedLine],
    words: &[TimedWord],
    custom_fillers: &[String],
) -> (Vec<TimedLine>, Vec<TimedWord>, Vec<TimedWord>) {
    let fillers = filler_set(custom_fillers);
    let mut clean_words = Vec::with_capacity(words.len());
    let mut hits = Vec::new();
    for w in words {
        if is_filler_word(&w.word, &fillers) {
            hits.push(w.clone());
        } else {
            clean_words.push(w.clone());
        }
    }
    let mut clean_lines = Vec::with_capacity(lines.len());
    for line in lines {
        let kept: Vec<&str> = line.text.split_whitespace().filter(|tok| !is_filler_word(tok, &fillers)).collect();
        if !kept.is_empty() {
            clean_lines.push(TimedLine { text: kept.join(" "), start: line.start, end: line.end });
        }
    }
    (clean_lines, clean_words, hits)
}

/// Render [`TimedLine`] entries as a valid SubRip (`.srt`) string via `filmcraft_captions`.
pub fn build_srt_from_lines(lines: &[TimedLine]) -> String {
    let cues: Vec<Cue> = lines
        .iter()
        .filter(|l| !l.text.trim().is_empty())
        .map(|l| {
            let start = Tick::from_seconds_f64(l.start.max(0.0));
            let end = Tick::from_seconds_f64(l.end.max(l.start + 0.05));
            Cue { start, end, text: l.text.trim().to_string(), ..Default::default() }
        })
        .collect();
    let doc = Document { cues, ..Default::default() };
    String::from_utf8_lossy(&filmcraft_captions::write(&doc, Format::Srt, WriteOptions::default())).into_owned()
}

/// Render [`TimedWord`] entries as word-by-word SubRip (`.srt`) captions via `filmcraft_captions`.
pub fn build_srt_from_words(words: &[TimedWord]) -> String {
    let lines: Vec<TimedLine> = words
        .iter()
        .filter(|w| !w.word.trim().is_empty())
        .map(|w| TimedLine { text: w.word.trim().to_string(), start: w.start, end: w.end })
        .collect();
    build_srt_from_lines(&lines)
}

/// Build the Gemini `generateContent` JSON request body (with or without `responseSchema`).
pub fn build_caption_request_body(audio_base64: &str, language: &str, natural_split: bool, words_per_line: usize, use_schema: bool) -> Value {
    let lang_instruction = if language.is_empty() || language == "auto" {
        "Detect the spoken language automatically.".to_string()
    } else {
        format!("The spoken language is {language}.")
    };
    let split_instruction = if natural_split {
        "Split \"lines\" naturally by sentence or clause — wherever a natural pause or breath in the speech happens — rather than by a fixed word count. Keep each line short and readable.".to_string()
    } else {
        let n = words_per_line.clamp(1, 20);
        format!("Break \"lines\" into short readable subtitle chunks of no more than {n} words per line.")
    };
    let prompt = format!(
        "Transcribe this audio. Respond with ONLY a single valid JSON object — no markdown, \
         no code fences, no extra text — with this exact shape:\n\
         {{\"lines\": [{{\"text\": string, \"start\": number, \"end\": number}}, ...], \
         \"words\": [{{\"word\": string, \"start\": number, \"end\": number}}, ...]}}\n\
         All times are in seconds from the start of the audio as plain numbers (e.g. 12.34).\n\
         \"words\" must include EVERY spoken word individually, in order, with its own start/end time — \
         including short filler words like \"يعني\", \"إمم\", \"آه\", \"طيب\", \"خلاص\".\n\
         {split_instruction} {lang_instruction}"
    );

    let mut gen_cfg = json!({
        "responseMimeType": "application/json",
    });
    if use_schema {
        gen_cfg["responseSchema"] = json!({
            "type": "OBJECT",
            "properties": {
                "lines": {
                    "type": "ARRAY",
                    "items": {
                        "type": "OBJECT",
                        "properties": {
                            "text": { "type": "STRING" },
                            "start": { "type": "NUMBER" },
                            "end": { "type": "NUMBER" }
                        },
                        "required": ["text", "start", "end"]
                    }
                },
                "words": {
                    "type": "ARRAY",
                    "items": {
                        "type": "OBJECT",
                        "properties": {
                            "word": { "type": "STRING" },
                            "start": { "type": "NUMBER" },
                            "end": { "type": "NUMBER" }
                        },
                        "required": ["word", "start", "end"]
                    }
                }
            },
            "required": ["lines", "words"]
        });
    }

    json!({
        "contents": [{
            "parts": [
                { "text": prompt },
                {
                    "inline_data": {
                        "mime_type": "audio/wav",
                        "data": audio_base64,
                    }
                }
            ]
        }],
        "generationConfig": gen_cfg,
    })
}

/// Extract concatenated text parts from a Gemini `generateContent` JSON envelope.
pub fn extract_candidate_text(envelope: &Value) -> Result<String> {
    if let Some(err) = envelope.get("error") {
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("Gemini API returned an error");
        let code = err.get("code").and_then(Value::as_u64).unwrap_or(0);
        return Err(EngineError::Other(if code > 0 { format!("Gemini API error ({code}): {msg}") } else { format!("Gemini API error: {msg}") }));
    }
    let parts = envelope
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| EngineError::Other("Gemini response contained no candidate content parts".into()))?;
    let mut joined = Vec::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(Value::as_str) {
            joined.push(t);
        }
    }
    let raw = joined.join("\n");
    if raw.trim().is_empty() {
        return Err(EngineError::Other("Gemini returned an empty response".into()));
    }
    Ok(raw)
}

/// Strip optional UTF-8 BOM and Markdown ```json ... ``` fences from a model response.
fn strip_code_fences(raw: &str) -> &str {
    let mut s = raw.trim().trim_start_matches('\u{FEFF}').trim();
    if let Some(rest) = s.strip_prefix("```") {
        // drop optional language tag (e.g. `json`) up to newline
        s = rest.find('\n').map_or(rest, |nl| &rest[nl + 1..]);
        if let Some(before) = s.rsplit_once("```").map(|(b, _)| b) {
            s = before;
        }
    }
    s.trim()
}

/// Parse Gemini's structured JSON caption text into normalized [`CaptionResult`].
pub fn parse_caption_json(raw: &str) -> Result<CaptionResult> {
    let clean = strip_code_fences(raw);
    let val: Value = serde_json::from_str(clean).map_err(|e| EngineError::Other(format!("could not parse Gemini JSON response: {e}")))?;

    let mut lines = Vec::new();
    if let Some(arr) = val.get("lines").and_then(Value::as_array) {
        for item in arr {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let start = item.get("start").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
            let end = item.get("end").and_then(Value::as_f64).unwrap_or(start + 0.5).max(start + 0.05);
            if !text.is_empty() && start.is_finite() && end.is_finite() {
                lines.push(TimedLine { text, start, end });
            }
        }
    }

    let mut words = Vec::new();
    if let Some(arr) = val.get("words").and_then(Value::as_array) {
        for item in arr {
            let word = item.get("word").or_else(|| item.get("text")).and_then(Value::as_str).unwrap_or("").trim().to_string();
            let start = item.get("start").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
            let end = item.get("end").and_then(Value::as_f64).unwrap_or(start + 0.1).max(start + 0.02);
            if !word.is_empty() && start.is_finite() && end.is_finite() {
                words.push(TimedWord { word, start, end });
            }
        }
    }

    lines.sort_by(|a, b| a.start.total_cmp(&b.start));
    words.sort_by(|a, b| a.start.total_cmp(&b.start));

    if lines.is_empty() && words.is_empty() {
        return Err(EngineError::Other("Gemini response contained no caption lines or words".into()));
    }
    // If only words were returned, synthesize lines by grouping up to 8 words
    if lines.is_empty() {
        for chunk in words.chunks(8) {
            if let (Some(first), Some(last)) = (chunk.first(), chunk.last()) {
                let text = chunk.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(" ");
                lines.push(TimedLine { text, start: first.start, end: last.end });
            }
        }
    }
    // If only lines were returned, distribute words evenly across each line
    if words.is_empty() {
        for l in &lines {
            let toks: Vec<&str> = l.text.split_whitespace().collect();
            let n = toks.len().max(1);
            let step = (l.end - l.start) / n as f64;
            for (i, tok) in toks.into_iter().enumerate() {
                let s = l.start + step * i as f64;
                let e = (s + step).max(s + 0.02);
                words.push(TimedWord { word: tok.to_string(), start: s, end: e });
            }
        }
    }

    Ok(CaptionResult { lines, words })
}

/// Concatenate audio spans into one WAV buffer and build a piece-wise offset table mapping
/// concatenated audio seconds back to sequence timeline seconds.
struct AudioTimelineMap {
    /// `(audio_offset_sec, duration_sec, timeline_start_sec)`
    segments: Vec<(f64, f64, f64)>,
    total_seconds: f64,
}

impl AudioTimelineMap {
    fn from_spans(spans: &[TimelineAudioSpan]) -> (Vec<f32>, Self) {
        let mut combined = Vec::new();
        let mut segments = Vec::new();
        let mut audio_cursor = 0.0_f64;
        for span in spans {
            let dur_sec = span.samples.len() as f64 / f64::from(span.sample_rate);
            let tl_start = span.start.0 as f64 / TICKS_PER_SECOND as f64;
            segments.push((audio_cursor, dur_sec, tl_start));
            combined.extend_from_slice(&span.samples);
            audio_cursor += dur_sec;
        }
        (combined, Self { segments, total_seconds: audio_cursor })
    }

    fn to_timeline_seconds(&self, audio_sec: f64) -> f64 {
        let t = audio_sec.clamp(0.0, self.total_seconds);
        for &(a_start, dur, tl_start) in &self.segments {
            if t <= a_start + dur + 1e-6 {
                return tl_start + (t - a_start).max(0.0);
            }
        }
        self.segments.last().map_or(t, |&(a_start, _, tl_start)| tl_start + (t - a_start).max(0.0))
    }
}

/// Execute a Gemini `generateContent` request with automatic schema-rejection fallback and
/// bounded retries on transient server errors (429 / 500 / 503 / 504).
fn call_gemini_with_fallback(
    transport: &dyn GeminiTransport,
    url: &str,
    api_key: &str,
    build_body: impl Fn(bool) -> Value,
) -> Result<Value> {
    let mut use_schema = true;
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let body = build_body(use_schema);
        let (status, text) = transport
            .post_json(url, api_key, &body)
            .map_err(|e| EngineError::Other(sanitize_secret(&e, api_key)))?;
        let parsed: Option<Value> = serde_json::from_str(&text).ok();

        if status == 200
            && let Some(env) = parsed.as_ref()
            && env.get("error").is_none()
        {
            return Ok(env.clone());
        }

        let err_msg = parsed
            .as_ref()
            .and_then(|v| v.pointer("/error/message"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                parsed
                    .as_ref()
                    .and_then(|v| v.pointer("/error/status"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or(text.as_str());
        let err_code = parsed
            .as_ref()
            .and_then(|v| v.pointer("/error/code"))
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(status));

        // Model rejected responseSchema -> retry once without responseSchema (same selected model)
        if use_schema
            && err_code == 400
            && (err_msg.contains("schema") || err_msg.contains("responseSchema") || err_msg.contains("response_schema"))
        {
            use_schema = false;
            attempt = attempt.saturating_sub(1);
            continue;
        }

        // Transient server error -> retry up to MAX_ATTEMPTS on the same selected model.
        // Permission / model-access errors (401/403/404) are never retried and never fall back
        // to another model; Google's message is surfaced verbatim.
        if matches!(err_code, 429 | 500 | 503 | 504) && attempt < MAX_ATTEMPTS {
            continue;
        }

        let safe = sanitize_secret(err_msg.trim(), api_key);
        let summary = if safe.is_empty() { format!("Gemini API error (HTTP {status})") } else { format!("Gemini API error ({err_code}): {safe}") };
        return Err(EngineError::Other(summary));
    }
}

fn active_transport(s: &Session) -> Result<Arc<dyn GeminiTransport>> {
    if let Some(t) = &s.gemini_transport {
        return Ok(t.clone());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Ok(Arc::new(UreqTransport))
    }
    #[cfg(target_arch = "wasm32")]
    {
        Err(EngineError::Other("Gemini HTTPS transport is not available in this build".into()))
    }
}

fn require_api_key(s: &Session) -> Result<String> {
    match s.secrets.get(GEMINI_API_KEY) {
        Ok(Some(k)) if !k.trim().is_empty() => Ok(k),
        Ok(_) => Err(EngineError::Other("Gemini API key is not configured. Open Settings ▸ Media Analysis & Transcription to add your key.".into())),
        Err(e) => Err(EngineError::Other(e)),
    }
}

// ------------------------------------------------------------------ Commands

fn set_api_key_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let key = str_p(p, "key").ok_or_else(|| bad("gemini.setApiKey", "`key` string is required (empty string clears the key)"))?;
    s.secrets.set(GEMINI_API_KEY, key).map_err(EngineError::Other)?;
    Ok(json!({
        "configured": s.secrets.is_configured(GEMINI_API_KEY),
        "persistent": s.secrets.is_persistent(),
        "backend": s.secrets.backend_label(),
        "masked": s.secrets.masked(GEMINI_API_KEY),
    }))
}

fn api_key_status_cmd(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(json!({
        "configured": s.secrets.is_configured(GEMINI_API_KEY),
        "persistent": s.secrets.is_persistent(),
        "backend": s.secrets.backend_label(),
        "masked": s.secrets.masked(GEMINI_API_KEY),
    }))
}

fn generate_captions_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let api_key = require_api_key(s)?;
    let model = str_p(p, "model")
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(&s.prefs.media_analysis.gemini_model)
        .to_string();
    let url = endpoint_url(&model)?;
    let language = str_p(p, "language").unwrap_or("auto");
    let word_by_word = bool_p(p, "wordByWord").unwrap_or(false);
    let remove_fillers = bool_p(p, "removeFillers").unwrap_or(false);
    let natural_split = bool_p(p, "naturalSplit").unwrap_or(true);
    let words_per_line = u64_p(p, "wordsPerLine").unwrap_or(8) as usize;
    let custom_fillers: Vec<String> = match p.get("customFillers").and_then(Value::as_array) {
        Some(arr) => arr.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None => s.prefs.media_analysis.custom_fillers.clone(),
    };
    let explicit_clips: Option<Vec<ClipId>> = p
        .get("clips")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).map(ClipId).collect())
        .or_else(|| u64_p(p, "clip").map(|c| vec![ClipId(c)]));

    let spans = extract_timeline_audio(s, explicit_clips.as_deref(), ANALYSIS_SAMPLE_RATE)?;
    let (mono, map) = AudioTimelineMap::from_spans(&spans);
    let wav = encode_wav_pcm16(&mono, ANALYSIS_SAMPLE_RATE);
    let audio_b64 = B64.encode(&wav);
    if audio_b64.len() >= MAX_INLINE_BASE64_BYTES {
        return Err(EngineError::Other(format!(
            "selected audio ({:.1} min) exceeds Gemini's inline upload limit; select a shorter clip or range",
            map.total_seconds / 60.0
        )));
    }

    let transport = active_transport(s)?;
    let env = call_gemini_with_fallback(transport.as_ref(), &url, &api_key, |use_schema| {
        build_caption_request_body(&audio_b64, language, natural_split, words_per_line, use_schema)
    })?;
    let raw_text = extract_candidate_text(&env)?;
    let mut parsed = parse_caption_json(&raw_text)?;

    // Map audio-relative seconds back to timeline seconds
    for l in &mut parsed.lines {
        l.start = map.to_timeline_seconds(l.start);
        l.end = map.to_timeline_seconds(l.end).max(l.start + 0.05);
    }
    for w in &mut parsed.words {
        w.start = map.to_timeline_seconds(w.start);
        w.end = map.to_timeline_seconds(w.end).max(w.start + 0.02);
    }

    // Always detect filler hits; remove them from the caption output when requested
    let (clean_lines, clean_words, filler_hits) = remove_fillers_from_captions(&parsed.lines, &parsed.words, &custom_fillers);
    let (final_lines, final_words) = if remove_fillers { (clean_lines, clean_words) } else { (parsed.lines.clone(), parsed.words.clone()) };

    let line_srt = build_srt_from_lines(&final_lines);
    let word_srt = build_srt_from_words(&final_words);
    let active_srt = if word_by_word { word_srt.clone() } else { line_srt.clone() };

    Ok(json!({
        "model": model,
        "language": language,
        "wordByWord": word_by_word,
        "removeFillers": remove_fillers,
        "audioSeconds": map.total_seconds,
        "lines": final_lines,
        "words": final_words,
        "rawLines": parsed.lines,
        "rawWords": parsed.words,
        "fillerHits": filler_hits,
        "srt": active_srt,
        "lineSrt": line_srt,
        "wordSrt": word_srt,
    }))
}

fn build_translation_request_body(texts: &[String], target_language: &str, use_schema: bool) -> Value {
    let input_json = serde_json::to_string(texts).unwrap_or_else(|_| "[]".to_string());
    let prompt = format!(
        "You translate subtitles into {target_language}.\n\
         Rules:\n\
         - Return exactly {} translations, in the same order, one per input line.\n\
         - Never merge, split, skip or reorder lines.\n\
         - Keep each translation short and natural for on-screen subtitles.\n\
         - Keep names, numbers and brand names. Do not add notes or explanations.\n\
         - If a line is empty, return an empty string for it.\n\n\
         Input (JSON array of {} strings):\n\
         {input_json}\n\n\
         Answer as JSON: {{\"translations\": [ ... ]}}",
        texts.len(),
        texts.len()
    );
    let mut gen_cfg = json!({ "responseMimeType": "application/json" });
    if use_schema {
        gen_cfg["responseSchema"] = json!({
            "type": "OBJECT",
            "properties": {
                "translations": {
                    "type": "ARRAY",
                    "items": { "type": "STRING" }
                }
            },
            "required": ["translations"]
        });
    }
    json!({
        "contents": [{ "parts": [{ "text": prompt }] }],
        "generationConfig": gen_cfg,
    })
}

fn parse_translations_json(raw: &str) -> Result<Vec<String>> {
    let clean = strip_code_fences(raw);
    let v: Value = serde_json::from_str(clean).map_err(|e| EngineError::Other(format!("could not parse Gemini translation JSON: {e}")))?;
    let list = v
        .as_array()
        .or_else(|| v.get("translations").and_then(Value::as_array))
        .ok_or_else(|| EngineError::Other("Gemini translation response missing `translations` array".into()))?;
    Ok(list.iter().map(|item| item.as_str().unwrap_or("").trim().to_string()).collect())
}

fn translate_srt_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let api_key = require_api_key(s)?;
    let srt = str_p(p, "srt").ok_or_else(|| bad("gemini.translateSrt", "`srt` is required"))?;
    let target_language = str_p(p, "targetLanguage")
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .ok_or_else(|| bad("gemini.translateSrt", "`targetLanguage` is required"))?;
    let model = str_p(p, "model")
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(&s.prefs.media_analysis.gemini_model)
        .to_string();
    let url = endpoint_url(&model)?;

    let mut doc = filmcraft_captions::parse(srt.as_bytes(), Format::Srt)
        .map_err(|e| EngineError::Other(format!("SRT preview is invalid: {e}")))?;
    if doc.cues.is_empty() {
        return Err(EngineError::Other("SRT preview has no captions to translate".into()));
    }

    let transport = active_transport(s)?;
    let originals: Vec<String> = doc.cues.iter().map(|c| c.text.clone()).collect();
    let mut translated_all = Vec::with_capacity(originals.len());
    for chunk in originals.chunks(TRANSLATION_BATCH_SIZE) {
        let env = call_gemini_with_fallback(transport.as_ref(), &url, &api_key, |use_schema| {
            build_translation_request_body(chunk, target_language, use_schema)
        })?;
        let raw = extract_candidate_text(&env)?;
        let batch = parse_translations_json(&raw)?;
        if batch.len() != chunk.len() {
            return Err(EngineError::Other(format!(
                "Gemini returned {} translated line(s) for a batch of {}",
                batch.len(),
                chunk.len()
            )));
        }
        translated_all.extend(batch);
    }

    for (cue, text) in doc.cues.iter_mut().zip(translated_all) {
        cue.text = text;
    }
    let out_srt = String::from_utf8_lossy(&filmcraft_captions::write(&doc, Format::Srt, WriteOptions::default())).into_owned();
    Ok(json!({
        "srt": out_srt,
        "cues": doc.cues.len(),
        "targetLanguage": target_language,
        "model": model,
    }))
}

fn import_srt_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let srt = str_p(p, "srt").ok_or_else(|| bad("gemini.importSrt", "`srt` is required"))?;
    if srt.trim().is_empty() {
        return Err(bad("gemini.importSrt", "`srt` cannot be empty"));
    }
    let name = str_p(p, "name").filter(|n| !n.trim().is_empty()).unwrap_or("Gemini Captions");
    let language = str_p(p, "language").filter(|l| !l.trim().is_empty() && *l != "auto");
    crate::captions::import(s, &format!("{name}.srt"), srt.as_bytes(), Format::Srt, Some(name), language)
}

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, params: &'static str, enabled: Enabled, run: Run, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu: &[], shortcut: None, params, enabled, run, journal, named: crate::commands::NamedKeys::NONE }
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        // `journal: false` guarantees the plaintext API key is NEVER recorded in `session.journal`.
        spec("gemini.setApiKey", "Set Gemini API Key", r#"{"key":str}"#, always, set_api_key_cmd, false),
        spec("gemini.apiKeyStatus", "Gemini API Key Status", "{}", always, api_key_status_cmd, false),
        spec(
            "gemini.generateCaptions",
            "Generate Captions with Gemini",
            r#"{"model":str?,"language":str?,"wordByWord":bool?,"removeFillers":bool?,"customFillers":[str]?,"naturalSplit":bool?,"wordsPerLine":n?,"clips":[clipId]?,"clip":clipId?}"#,
            has_seq,
            generate_captions_cmd,
            false,
        ),
        spec(
            "gemini.translateSrt",
            "Translate Captions with Gemini",
            r#"{"srt":str,"targetLanguage":str,"model":str?}"#,
            always,
            translate_srt_cmd,
            false,
        ),
        spec(
            "gemini.importSrt",
            "Import SRT Captions to Timeline",
            r#"{"srt":str,"name":str?,"language":str?}"#,
            always,
            import_srt_cmd,
            true,
        ),
    ]
}
