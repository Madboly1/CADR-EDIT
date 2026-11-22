//! Audio silence detection and undo-safe A/V-synced silence cutting (`silence.*`).
//!
//! Complements `transcript.removePauses` (which finds pauses between transcribed words) by
//! analyzing the actual audio waveform (windowed RMS in dBFS) of the selected clip(s) or active
//! sequence — working without needing a transcript.
//!
//! Preview (`silence.detect`) is read-only and non-destructive. Applying (`silence.apply`) reuses
//! [`filmcraft_edit::transcript::ripple_delete_ranges`] inside a single `edit_sequence` transaction
//! so video, audio, and caption tracks stay in sync and a single Undo restores the timeline.

use serde_json::{Value, json};

use filmcraft_edit::transcript::{merge_ranges, ripple_delete_ranges};
use filmcraft_project::{ClipId, Sequence, TrackItem};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick, TimeRange};

use crate::commands::{CommandSpec, bad, f64_p, has_seq};
use crate::{EngineError, Result, Session};

/// Analysis sample rate (16 kHz mono, matching `filmcraft_speech::SAMPLE_RATE`).
pub const ANALYSIS_SAMPLE_RATE: u32 = filmcraft_speech::SAMPLE_RATE;

/// Analysis window duration in milliseconds (10 ms = 160 samples at 16 kHz).
const WINDOW_MS: usize = 10;

pub const DEFAULT_THRESHOLD_DB: f64 = -35.0;
pub const DEFAULT_MIN_SILENCE_SECONDS: f64 = 0.5;
pub const DEFAULT_PADDING_SECONDS: f64 = 0.10;

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal, named: crate::commands::NamedKeys::NONE }
}

/// Parameters controlling waveform silence detection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SilenceOptions {
    /// RMS level (dBFS) at or below which audio counts as silence (e.g. `-35.0`).
    pub threshold_db: f64,
    /// Minimum contiguous silent duration in seconds before a range is detected.
    pub min_silence_seconds: f64,
    /// Padding (seconds) kept on each side of speech so cuts never clip word attacks/tails.
    pub padding_seconds: f64,
}

impl Default for SilenceOptions {
    fn default() -> Self {
        Self {
            threshold_db: DEFAULT_THRESHOLD_DB,
            min_silence_seconds: DEFAULT_MIN_SILENCE_SECONDS,
            padding_seconds: DEFAULT_PADDING_SECONDS,
        }
    }
}

impl SilenceOptions {
    pub fn clamped(self) -> Self {
        Self {
            threshold_db: self.threshold_db.clamp(-90.0, -1.0),
            min_silence_seconds: self.min_silence_seconds.clamp(0.05, 60.0),
            padding_seconds: self.padding_seconds.clamp(0.0, 10.0),
        }
    }
}

/// One timeline-positioned mono audio span extracted from selected clip(s) or the sequence.
#[derive(Clone, Debug)]
pub struct TimelineAudioSpan {
    /// Sequence start tick of `samples[0]`.
    pub start: Tick,
    /// Sequence duration in ticks covered by `samples`.
    pub duration: Tick,
    /// Mono samples at `sample_rate` Hz.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// Resolve audio track items to analyze: explicit `clips`, else selected timeline clips (resolving
/// linked video clips to their linked audio clips), else all enabled audio clips in the sequence.
pub(crate) fn target_audio_clips(seq: &Sequence, explicit: Option<&[ClipId]>, selection: &[ClipId]) -> Vec<TrackItem> {
    let pick_ids = match explicit {
        Some(ids) if !ids.is_empty() => ids,
        _ => selection,
    };
    let mut out: Vec<TrackItem> = Vec::new();
    fn push_unique(out: &mut Vec<TrackItem>, it: &TrackItem) {
        if it.enabled && !it.reverse && it.frame_hold.is_none() && it.speed > 0.0 && !out.iter().any(|x| x.id == it.id) {
            out.push(it.clone());
        }
    }

    if !pick_ids.is_empty() {
        for &cid in pick_ids {
            // Direct audio clip selection
            if let Some(it) = seq.audio_tracks.iter().flat_map(|t| &t.items).find(|it| it.id == cid) {
                push_unique(&mut out, it);
                continue;
            }
            // Selected video clip -> follow its link group to any enabled audio clips
            if let Some(vit) = seq.video_tracks.iter().flat_map(|t| &t.items).find(|it| it.id == cid)
                && let Some(link) = vit.link
            {
                for ait in seq.audio_tracks.iter().flat_map(|t| &t.items).filter(|a| a.link == Some(link)) {
                    push_unique(&mut out, ait);
                }
            }
        }
    }

    if out.is_empty() && explicit.is_none() {
        for track in &seq.audio_tracks {
            if track.muted {
                continue;
            }
            for it in &track.items {
                push_unique(&mut out, it);
            }
        }
    }
    out.sort_by_key(|it| it.start);
    out
}

/// Extract timeline-aligned mono audio spans from the selected clip(s) or active sequence.
/// Overlapping clips are summed into contiguous spans so silence detection and Gemini
/// transcription see the true audible mix and accurate timeline offsets.
pub(crate) fn extract_timeline_audio(s: &Session, explicit_clips: Option<&[ClipId]>, sample_rate: u32) -> Result<Vec<TimelineAudioSpan>> {
    let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let clips = target_audio_clips(seq, explicit_clips, &s.state.selection);
    if clips.is_empty() {
        return Err(EngineError::Other("no enabled audio clips found in the selection or sequence".into()));
    }

    // Group overlapping/touching clips into contiguous timeline windows
    let mut windows: Vec<(Tick, Tick, Vec<TrackItem>)> = Vec::new();
    for clip in clips {
        if clip.duration <= Tick::ZERO {
            continue;
        }
        let end = clip.end();
        match windows.last_mut() {
            Some(last) if clip.start <= last.1 => {
                last.1 = last.1.max(end);
                last.2.push(clip);
            }
            _ => windows.push((clip.start, end, vec![clip])),
        }
    }

    let mut spans = Vec::new();
    for (win_start, win_end, group) in windows {
        let win_dur = win_end - win_start;
        let total_samples = win_dur.to_units_floor(sample_rate as i64).max(0) as usize;
        if total_samples == 0 {
            continue;
        }
        let mut mix = vec![0.0_f32; total_samples];
        let mut decoded_any = false;
        for clip in &group {
            let Some(src) = s.source(clip.item) else { continue };
            if !src.info().has_audio() {
                continue;
            }
            let src_start = clip.source_in.to_units_floor(sample_rate as i64).max(0);
            let src_ticks = Tick(((clip.duration.0 as f64) * clip.speed).round() as i64);
            let src_len = src_ticks.to_units_floor(sample_rate as i64).max(0) as usize;
            if src_len == 0 {
                continue;
            }
            let Ok(buf) = src.audio(src_start, src_len, sample_rate) else { continue };
            let mono = filmcraft_speech::downmix(&buf.channels);
            if mono.is_empty() {
                continue;
            }
            decoded_any = true;
            let offset = (clip.start - win_start).to_units_floor(sample_rate as i64).max(0) as usize;
            let tl_len = clip.duration.to_units_floor(sample_rate as i64).max(0) as usize;
            let speed = clip.speed;
            for i in 0..tl_len {
                let dst = offset + i;
                if dst >= mix.len() {
                    break;
                }
                let src_idx = if (speed - 1.0).abs() < 1e-6 { i } else { ((i as f64) * speed).round() as usize };
                if let Some(&sample) = mono.get(src_idx) {
                    mix[dst] += sample;
                }
            }
        }
        if decoded_any {
            // Clamp mix to [-1.0, 1.0] to prevent clipping when multiple tracks overlap
            for v in &mut mix {
                *v = v.clamp(-1.0, 1.0);
            }
            spans.push(TimelineAudioSpan { start: win_start, duration: win_dur, samples: mix, sample_rate });
        }
    }

    if spans.is_empty() {
        return Err(EngineError::Other("selected clip(s) contain no decodable audio".into()));
    }
    Ok(spans)
}

/// Detect silent ranges in mono `samples` (at `sample_rate`), mapped to sequence time starting at
/// `span_start`, with inward padding and inward frame snapping so speech is never clipped.
pub fn detect_silence_in_samples(samples: &[f32], sample_rate: u32, span_start: Tick, opts: SilenceOptions, rate: FrameRate) -> Vec<TimeRange> {
    let opts = opts.clamped();
    let win = ((sample_rate as usize) * WINDOW_MS / 1000).max(1);
    if samples.is_empty() {
        return Vec::new();
    }
    let threshold_lin = 10.0_f64.powf(opts.threshold_db / 20.0);
    let min_windows = ((opts.min_silence_seconds * 1000.0) / WINDOW_MS as f64).ceil().max(1.0) as usize;
    let total_windows = samples.len().div_ceil(win);

    let mut raw: Vec<(f64, f64)> = Vec::new();
    let mut run_start: Option<usize> = None;

    for wi in 0..total_windows {
        let a = wi * win;
        let b = (a + win).min(samples.len());
        let slice = &samples[a..b];
        let sum_sq: f64 = slice.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
        let rms = (sum_sq / slice.len().max(1) as f64).sqrt();
        let is_silent = rms <= threshold_lin;

        if is_silent {
            if run_start.is_none() {
                run_start = Some(wi);
            }
        } else if let Some(s_wi) = run_start.take() {
            if wi - s_wi >= min_windows {
                let s_sec = (s_wi * win) as f64 / f64::from(sample_rate);
                let e_sec = (wi * win).min(samples.len()) as f64 / f64::from(sample_rate);
                raw.push((s_sec, e_sec));
            }
        }
    }
    if let Some(s_wi) = run_start
        && total_windows - s_wi >= min_windows
    {
        let s_sec = (s_wi * win) as f64 / f64::from(sample_rate);
        let e_sec = samples.len() as f64 / f64::from(sample_rate);
        raw.push((s_sec, e_sec));
    }

    let pad = Tick::from_seconds_f64(opts.padding_seconds);
    let fd = rate.frame_duration();
    let mut out = Vec::new();
    for (s_sec, e_sec) in raw {
        let a = span_start + Tick::from_seconds_f64(s_sec);
        let b = span_start + Tick::from_seconds_f64(e_sec);
        let cut_s = a + pad;
        let cut_e = b - pad;
        if cut_e <= cut_s {
            continue;
        }
        // Snap inward to sequence frame boundaries so adjacent speech frames stay intact
        let mut s2 = rate.snap(cut_s);
        if s2 < cut_s {
            s2 += fd;
        }
        let e2 = rate.snap(cut_e);
        if e2 > s2 {
            out.push(TimeRange::from_bounds(s2, e2));
        }
    }
    merge_ranges(out)
}

fn clips_param(p: &Value) -> Option<Vec<ClipId>> {
    p.get("clips")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).map(ClipId).collect())
}

fn options_param(p: &Value) -> SilenceOptions {
    SilenceOptions {
        threshold_db: f64_p(p, "thresholdDb").unwrap_or(DEFAULT_THRESHOLD_DB),
        min_silence_seconds: f64_p(p, "minSilenceSeconds").unwrap_or(DEFAULT_MIN_SILENCE_SECONDS),
        padding_seconds: f64_p(p, "paddingSeconds").unwrap_or(DEFAULT_PADDING_SECONDS),
    }
    .clamped()
}

/// Detect silent ranges across selected clip(s) or active sequence audio (non-destructive).
pub fn detect_ranges(s: &Session, explicit_clips: Option<&[ClipId]>, opts: SilenceOptions) -> Result<Vec<TimeRange>> {
    let rate = s.sequence_rate();
    let spans = extract_timeline_audio(s, explicit_clips, ANALYSIS_SAMPLE_RATE)?;
    let mut all = Vec::new();
    for span in &spans {
        all.extend(detect_silence_in_samples(&span.samples, span.sample_rate, span.start, opts, rate));
    }
    Ok(merge_ranges(all))
}

fn detect_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let opts = options_param(p);
    let clips = clips_param(p);
    let ranges = detect_ranges(s, clips.as_deref(), opts)?;
    let total_ticks: i64 = ranges.iter().map(|r| r.duration.0).sum();
    let list: Vec<Value> = ranges
        .iter()
        .map(|r| {
            let start_sec = r.start.0 as f64 / TICKS_PER_SECOND as f64;
            let end_sec = r.end().0 as f64 / TICKS_PER_SECOND as f64;
            let dur_sec = r.duration.0 as f64 / TICKS_PER_SECOND as f64;
            json!({
                "start": r.start.0,
                "end": r.end().0,
                "startSec": start_sec,
                "endSec": end_sec,
                "durationSec": dur_sec,
            })
        })
        .collect();
    Ok(json!({
        "ranges": list,
        "count": ranges.len(),
        "totalTicks": total_ticks,
        "totalSeconds": total_ticks as f64 / TICKS_PER_SECOND as f64,
        "thresholdDb": opts.threshold_db,
        "minSilenceSeconds": opts.min_silence_seconds,
        "paddingSeconds": opts.padding_seconds,
    }))
}

fn parse_explicit_ranges(p: &Value) -> Result<Option<Vec<TimeRange>>> {
    let Some(arr) = p.get("ranges").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item.get("start").and_then(Value::as_i64).ok_or_else(|| bad("silence.apply", "each range needs integer `start` and `end` ticks"))?;
        let e = item.get("end").and_then(Value::as_i64).ok_or_else(|| bad("silence.apply", "each range needs integer `start` and `end` ticks"))?;
        if e > s {
            out.push(TimeRange::from_bounds(Tick(s), Tick(e)));
        }
    }
    Ok(Some(merge_ranges(out)))
}

fn apply_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let ranges = match parse_explicit_ranges(p)? {
        Some(r) => r,
        None => {
            let opts = options_param(p);
            let clips = clips_param(p);
            detect_ranges(s, clips.as_deref(), opts)?
        }
    };
    let count = ranges.len();
    if count == 0 {
        return Ok(json!({"removed": 0, "ticks": 0, "seconds": 0.0}));
    }
    // Reuses the native ripple-delete primitive across all unlocked video, audio, and caption
    // tracks right-to-left in one undoable `edit_sequence` transaction.
    let total = s.edit_sequence("Cut Silences", |q, ctx, _| Ok(ripple_delete_ranges(q, ranges, ctx)))?;
    Ok(json!({
        "removed": count,
        "ticks": total.0,
        "seconds": total.0 as f64 / TICKS_PER_SECOND as f64,
    }))
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "silence.detect",
            "Detect Silences",
            &[],
            r#"{"thresholdDb":f?,"minSilenceSeconds":f?,"paddingSeconds":f?,"clips":[clipId]?}"#,
            has_seq,
            detect_cmd,
            false,
        ),
        spec(
            "silence.apply",
            "Cut Silences",
            &[],
            r#"{"ranges":[{"start":tick,"end":tick}]?,"thresholdDb":f?,"minSilenceSeconds":f?,"paddingSeconds":f?,"clips":[clipId]?}"#,
            has_seq,
            apply_cmd,
            true,
        ),
    ]
}
