use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};

use filmcraft_captions::Format;
use filmcraft_media::{Generator, MediaSource};
use filmcraft_project::{ClipId, ItemKind, Label, MediaClip, MediaRef, SequenceSettings, TrackKind};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick, TimeRange};

use crate::Session;
use crate::autosave::AutosaveConfig;
use crate::gemini::{GeminiTransport, encode_wav_pcm16, normalize_filler_token, sanitize_secret};
use crate::media_test_util::tmp_dir;
use crate::secret_store::{GEMINI_API_KEY, SecretStore, mask_secret};
use crate::silence::{ANALYSIS_SAMPLE_RATE, SilenceOptions, detect_silence_in_samples};

#[derive(Clone)]
struct RecordedCall {
    url: String,
    api_key: String,
    body: Value,
}

struct MockTransport {
    calls: Mutex<Vec<RecordedCall>>,
    responses: Mutex<Vec<std::result::Result<(u16, String), String>>>,
}

impl MockTransport {
    fn new(responses: Vec<std::result::Result<(u16, String), String>>) -> Arc<Self> {
        Arc::new(Self { calls: Mutex::new(Vec::new()), responses: Mutex::new(responses) })
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl GeminiTransport for MockTransport {
    fn post_json(&self, url: &str, api_key: &str, body: &Value) -> std::result::Result<(u16, String), String> {
        self.calls.lock().unwrap().push(RecordedCall { url: url.to_string(), api_key: api_key.to_string(), body: body.clone() });
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() { Err("unexpected extra Gemini call in test".into()) } else { q.remove(0) }
    }
}

fn gemini_ok_envelope(inner_json: &Value) -> (u16, String) {
    let text = serde_json::to_string(inner_json).unwrap();
    let env = json!({
        "candidates": [{
            "content": {
                "parts": [{ "text": text }]
            }
        }]
    });
    (200, serde_json::to_string(&env).unwrap())
}

/// Build mono 16 kHz PCM samples from `(duration_sec, amplitude)` segments.
/// `amplitude > 0.0` generates a 440 Hz sine wave of that peak amplitude; `0.0` is pure digital silence.
fn synth_samples(segments: &[(f64, f32)], sample_rate: u32) -> Vec<f32> {
    let mut out = Vec::new();
    let mut sample_idx = 0usize;
    for &(dur_s, amp) in segments {
        let count = (dur_s * f64::from(sample_rate)).round() as usize;
        for _ in 0..count {
            let t = sample_idx as f64 / f64::from(sample_rate);
            let v = if amp > 0.0 { amp * (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 } else { 0.0 };
            out.push(v);
            sample_idx += 1;
        }
    }
    out
}

/// Create a session with a synthetic WAV audio clip linked to a matching video clip on V1/A1.
/// Returns `(session, video_clip_id, audio_clip_id, temp_dir)`.
fn session_with_linked_wav(name: &str, segments: &[(f64, f32)]) -> (Session, ClipId, ClipId, std::path::PathBuf) {
    let dir = tmp_dir(name);
    let wav_path = dir.join("fixture.wav");
    let samples = synth_samples(segments, ANALYSIS_SAMPLE_RATE);
    let wav = encode_wav_pcm16(&samples, ANALYSIS_SAMPLE_RATE);
    std::fs::write(&wav_path, wav).unwrap();

    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [wav_path.to_string_lossy()]})).unwrap();
    let audio_item_id = filmcraft_project::ItemId(r["items"][0].as_u64().unwrap());
    let dur = s.project.item(audio_item_id).unwrap().duration();
    assert!(dur > Tick::ZERO, "imported WAV should have positive duration");

    let rate = FrameRate::FPS_25; // clean integer frame duration (1/25 s = 40 ms)
    let dur_snapped = rate.snap(dur).max(rate.frame_duration());
    let gen_kind = Generator::BlackVideo;
    let vsrc = filmcraft_media::generators::GeneratorSource::new(gen_kind.clone(), 640, 360, rate, dur_snapped);
    let vinfo = vsrc.info().clone();

    let mut p = (*s.project).clone();
    let video_clip = MediaClip {
        media: MediaRef::Generator(gen_kind),
        info: vinfo,
        interpret: Default::default(),
        mark_in: None,
        mark_out: None,
        markers: vec![],
        offline: false,
        proxy: None,
        identity: None,
    };
    let video_item_id = p.add_item("Camera 1", Label::Iris, ItemKind::Media(Box::new(video_clip)), None);
    let seq_id = p.new_sequence("Edit 1", SequenceSettings { width: 640, height: 360, frame_rate: rate, ..Default::default() }, 1, 1, None);
    let range = TimeRange::new(Tick::ZERO, dur_snapped);
    let mut vi = p.make_track_item(video_item_id, TrackKind::Video, Tick::ZERO, range, rate).unwrap();
    let mut ai = p.make_track_item(audio_item_id, TrackKind::Audio, Tick::ZERO, range, rate).unwrap();
    let link = p.next_id;
    p.next_id += 1;
    vi.link = Some(link);
    ai.link = Some(link);
    let v_id = vi.id;
    let a_id = ai.id;
    let q = p.sequence_mut(seq_id).unwrap();
    q.video_tracks[0].items.push(vi);
    q.audio_tracks[0].items.push(ai);
    q.check().unwrap();
    s.project = Arc::new(p);
    s.state.active_sequence = Some(seq_id);

    (s, v_id, a_id, dir)
}

#[test]
fn secret_store_never_leaks_plaintext_and_writes_nothing_on_session_only_platforms() {
    let dir = tmp_dir("secret-store");
    let secret = "AIzaSy-TEST-SYNTHETIC-KEY-9876543210";

    let mut store = SecretStore::for_dir(dir.clone());
    assert!(!store.is_configured(GEMINI_API_KEY));
    store.set(GEMINI_API_KEY, secret).unwrap();
    assert!(store.is_configured(GEMINI_API_KEY));
    assert_eq!(store.get(GEMINI_API_KEY).unwrap().as_deref(), Some(secret));

    #[cfg(target_os = "windows")]
    {
        assert!(store.is_persistent());
        // Reload from DPAPI-protected secrets.json in a fresh instance
        let store2 = SecretStore::for_dir(dir.clone());
        assert_eq!(store2.get(GEMINI_API_KEY).unwrap().as_deref(), Some(secret));
        let secrets_raw = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
        assert!(!secrets_raw.contains(secret), "plaintext secret leaked into secrets.json!");
        assert!(secrets_raw.contains("cadr.dpapi.v1:"), "expected DPAPI prefix in secrets.json");
    }

    #[cfg(not(target_os = "windows"))]
    {
        assert!(!store.is_persistent(), "non-Windows platforms must be session-only");
        assert!(store.backend_label().to_ascii_lowercase().contains("session"), "backend label must indicate session-only storage: {}", store.backend_label());
        // Nothing may be written to the data directory (no secrets.json, no .secret.salt)
        assert!(!dir.join("secrets.json").exists(), "secrets.json must not be created on session-only platforms");
        assert!(!dir.join(".secret.salt").exists(), ".secret.salt must not be created on session-only platforms");
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(entries.is_empty(), "data_dir must remain completely empty after SecretStore::set, found {entries:?}");

        // A fresh SecretStore pointed at the same dir must NOT see the session-only secret
        let store2 = SecretStore::for_dir(dir.clone());
        assert_eq!(store2.get(GEMINI_API_KEY).unwrap(), None, "session-only key must not persist across instances");
    }

    // Verify masking and error sanitization never expose the full secret
    let masked = mask_secret(secret);
    assert!(!masked.contains("SYNTHETIC"));
    assert!(masked.ends_with("3210"));
    assert_eq!(sanitize_secret(&format!("failed with {secret} here"), secret), "failed with [REDACTED] here");

    // Verify Session command `gemini.setApiKey` never writes plaintext to preferences.json or session.journal
    let mut s = Session::default();
    let cfg = AutosaveConfig::new(dir.clone());
    s.start_autosave(cfg).unwrap();
    let res = s.execute("gemini.setApiKey", json!({"key": secret})).unwrap();
    assert_eq!(res["configured"], true);
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(res["persistent"], false);
        assert!(!dir.join("secrets.json").exists());
        assert!(!dir.join(".secret.salt").exists());
    }
    assert!(s.journal.is_empty(), "gemini.setApiKey must have journal=false so secrets never enter journal");
    let _ = s.execute("prefs.set", json!({"key": "mediaAnalysis.geminiModel", "value": "gemini-3.6-flash"})).unwrap();
    if let Ok(prefs_raw) = std::fs::read_to_string(dir.join("preferences.json")) {
        assert!(!prefs_raw.contains(secret), "API key must never be persisted in preferences.json");
    }

    // Clear key
    let cleared = s.execute("gemini.setApiKey", json!({"key": ""})).unwrap();
    assert_eq!(cleared["configured"], false);
    assert_eq!(s.secrets.get(GEMINI_API_KEY).unwrap(), None);
    s.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gemini_captions_end_to_end_mocked_transport_and_arabic_fillers() {
    let (mut s, v_id, _a_id, dir) = session_with_linked_wav("gemini-captions", &[(2.5, 0.4)]);
    // Select only the video clip; Gemini command must resolve its linked audio clip
    s.state.selection = vec![v_id];

    let api_key = "AIzaSy-MOCK-ONLY-KEY-0001";
    s.execute("gemini.setApiKey", json!({"key": api_key})).unwrap();
    s.execute("prefs.set", json!({"key": "mediaAnalysis.geminiModel", "value": "gemini-3.5-flash-lite"})).unwrap();

    // Verify Arabic normalization handles tashkeel, alef variants, taa marbuta, alef maqsura
    assert_eq!(normalize_filler_token("يَعْنِي،"), normalize_filler_token("يعني"));
    assert_eq!(normalize_filler_token("آه!"), normalize_filler_token("اه"));
    assert_eq!(normalize_filler_token("إمم..."), normalize_filler_token("امم"));

    let mock_payload = json!({
        "lines": [
            { "text": "يعني أهلا بكم في كادر ستوديو", "start": 0.1, "end": 1.2 },
            { "text": "إمم هذا يَعْنِي actually تجربة رائعة", "start": 1.3, "end": 2.3 }
        ],
        "words": [
            { "word": "يعني", "start": 0.1, "end": 0.25 },
            { "word": "أهلا", "start": 0.25, "end": 0.5 },
            { "word": "بكم", "start": 0.5, "end": 0.7 },
            { "word": "في", "start": 0.7, "end": 0.85 },
            { "word": "كادر", "start": 0.85, "end": 1.0 },
            { "word": "ستوديو", "start": 1.0, "end": 1.2 },
            { "word": "إمم", "start": 1.3, "end": 1.45 },
            { "word": "هذا", "start": 1.45, "end": 1.6 },
            { "word": "يَعْنِي", "start": 1.6, "end": 1.75 },
            { "word": "actually", "start": 1.75, "end": 1.9 },
            { "word": "تجربة", "start": 1.9, "end": 2.1 },
            { "word": "رائعة", "start": 2.1, "end": 2.3 }
        ]
    });
    let transport = MockTransport::new(vec![Ok(gemini_ok_envelope(&mock_payload))]);
    s.gemini_transport = Some(transport.clone());

    let res = s
        .execute(
            "gemini.generateCaptions",
            json!({
                "language": "ar",
                "wordByWord": true,
                "removeFillers": true,
                "customFillers": ["actually"],
            }),
        )
        .unwrap();

    // Check transport request details: URL, header key, no key in URL, WAV inline_data, responseSchema
    let calls = transport.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].url, "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.5-flash-lite:generateContent");
    assert!(!calls[0].url.contains("key="), "API key must never be placed in the URL");
    assert_eq!(calls[0].api_key, api_key);
    assert!(calls[0].body["generationConfig"]["responseSchema"].is_object());
    assert!(!calls[0].body["contents"][0]["parts"][0]["text"].as_str().unwrap_or("").is_empty());
    let inline = &calls[0].body["contents"][0]["parts"][1]["inline_data"];
    assert_eq!(inline["mime_type"], "audio/wav");
    let decoded_wav = B64.decode(inline["data"].as_str().unwrap()).unwrap();
    assert_eq!(&decoded_wav[0..4], b"RIFF");
    assert_eq!(&decoded_wav[8..12], b"WAVE");

    // Check Arabic default + custom filler removal
    assert_eq!(res["fillerHits"].as_array().unwrap().len(), 4, "expected 4 filler hits: {}", res["fillerHits"]);
    let line_srt = res["lineSrt"].as_str().unwrap();
    assert!(!line_srt.contains("يعني"));
    assert!(!line_srt.contains("إمم"));
    assert!(!line_srt.contains("actually"));
    assert!(line_srt.contains("أهلا بكم في كادر ستوديو"));
    assert!(line_srt.contains("هذا تجربة رائعة"));

    // wordByWord=true selects wordSrt as active `srt`
    let word_srt = res["srt"].as_str().unwrap();
    let parsed_words = filmcraft_captions::parse(word_srt.as_bytes(), Format::Srt).unwrap();
    assert_eq!(parsed_words.cues.len(), 8, "12 words minus 4 fillers = 8 word cues");
    assert_eq!(parsed_words.cues[0].text, "أهلا");
    assert_eq!(parsed_words.cues[7].text, "رائعة");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gemini_captions_schema_fallback_retry_and_missing_key_error() {
    let (mut s, _v_id, _a_id, dir) = session_with_linked_wav("gemini-retry", &[(1.0, 0.3)]);
    let transport = MockTransport::new(vec![
        Ok((400, json!({"error": {"code": 400, "message": "Invalid JSON payload: responseSchema unsupported"}}).to_string())),
        Ok((429, json!({"error": {"code": 429, "message": "Rate limit exceeded"}}).to_string())),
        Ok(gemini_ok_envelope(&json!({
            "lines": [{ "text": "Hello CADR Studio", "start": 0.0, "end": 0.9 }],
            "words": [
                { "word": "Hello", "start": 0.0, "end": 0.3 },
                { "word": "CADR", "start": 0.3, "end": 0.6 },
                { "word": "Studio", "start": 0.6, "end": 0.9 }
            ]
        }))),
    ]);
    s.gemini_transport = Some(transport.clone());

    // Without API key configured, command fails cleanly without calling transport
    let missing = s.execute("gemini.generateCaptions", json!({}));
    assert!(missing.is_err());
    assert!(transport.calls().is_empty());

    // Configure key and retry: 400 schema rejection -> drops responseSchema -> 429 transient -> 200 OK
    s.execute("gemini.setApiKey", json!({"key": "AIzaSy-RETRY-KEY"})).unwrap();
    let res = s.execute("gemini.generateCaptions", json!({"model": "gemini-3.8-flash"})).unwrap();
    assert!(res["srt"].as_str().unwrap().contains("Hello CADR Studio"));

    let calls = transport.calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].body["generationConfig"].get("responseSchema").is_some(), "attempt 1 uses responseSchema");
    assert!(calls[1].body["generationConfig"].get("responseSchema").is_none(), "attempt 2 retries without responseSchema");
    assert!(calls[2].body["generationConfig"].get("responseSchema").is_none(), "attempt 3 retries after 429");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gemini_model_access_rejection_returns_verbatim_error_without_fallback() {
    // Verify the legacy model label warns about prior 2.5 account usage
    let legacy_label = crate::gemini::GEMINI_MODELS
        .iter()
        .find(|(id, _)| *id == "gemini-2.5-flash-lite")
        .map(|(_, label)| *label)
        .expect("gemini-2.5-flash-lite present in GEMINI_MODELS");
    assert!(legacy_label.contains("prior 2.5 usage"), "expected prior 2.5 usage notice in label: {legacy_label}");

    let (mut s, _v_id, _a_id, dir) = session_with_linked_wav("gemini-access-denied", &[(1.0, 0.3)]);
    s.execute("gemini.setApiKey", json!({"key": "AIzaSy-FRESH-KEY-0001"})).unwrap();
    s.execute("prefs.set", json!({"key": "mediaAnalysis.geminiModel", "value": "gemini-2.5-flash-lite"})).unwrap();

    let google_msg = "Permission denied: models/gemini-2.5-flash-lite is restricted to accounts with prior 2.5 usage.";
    let transport = MockTransport::new(vec![Ok((403, json!({"error": {"code": 403, "status": "PERMISSION_DENIED", "message": google_msg}}).to_string()))]);
    s.gemini_transport = Some(transport.clone());

    let err = s.execute("gemini.generateCaptions", json!({})).unwrap_err().to_string();
    assert!(err.contains(google_msg), "error must include Google's verbatim message, got: {err}");
    assert_eq!(transport.calls().len(), 1, "must NOT silently fall back to another model or retry 403");
    assert_eq!(transport.calls()[0].url, "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-lite:generateContent");
    assert_eq!(s.prefs.media_analysis.gemini_model, "gemini-2.5-flash-lite", "selected model must remain unchanged");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gemini_edit_srt_translate_and_import_undo_safe() {
    let (mut s, _v_id, _a_id, dir) = session_with_linked_wav("gemini-translate", &[(3.0, 0.3)]);
    s.execute("gemini.setApiKey", json!({"key": "AIzaSy-TRANS-KEY"})).unwrap();

    // User manually edited the SRT preview before translating / importing
    let edited_srt = "1\n00:00:00,200 --> 00:00:01,400\nمرحبا بكم في كادر\n\n2\n00:00:01,500 --> 00:00:02,800\nتحرير سريع وسهل\n";
    let transport = MockTransport::new(vec![Ok(gemini_ok_envelope(&json!({
        "translations": ["Welcome to CADR", "Fast and easy editing"]
    })))]);
    s.gemini_transport = Some(transport.clone());

    let translated = s
        .execute(
            "gemini.translateSrt",
            json!({
                "srt": edited_srt,
                "targetLanguage": "English",
            }),
        )
        .unwrap();
    let out_srt = translated["srt"].as_str().unwrap();

    // Timestamps must match original SRT cues 1-to-1
    let orig_doc = filmcraft_captions::parse(edited_srt.as_bytes(), Format::Srt).unwrap();
    let trans_doc = filmcraft_captions::parse(out_srt.as_bytes(), Format::Srt).unwrap();
    assert_eq!(orig_doc.cues.len(), trans_doc.cues.len());
    for (o, t) in orig_doc.cues.iter().zip(&trans_doc.cues) {
        assert_eq!(o.start, t.start);
        assert_eq!(o.end, t.end);
    }
    assert_eq!(trans_doc.cues[0].text, "Welcome to CADR");
    assert_eq!(trans_doc.cues[1].text, "Fast and easy editing");

    // Import translated SRT onto timeline as a native CaptionTrack (single undo step)
    assert!(s.active_sequence().unwrap().caption_tracks.is_empty());
    let imported = s.execute("gemini.importSrt", json!({"srt": out_srt, "name": "Gemini English", "language": "en"})).unwrap();
    assert_eq!(imported["captions"], 2);
    let q = s.active_sequence().unwrap();
    assert_eq!(q.caption_tracks.len(), 1);
    assert_eq!(q.caption_tracks[0].captions.len(), 2);
    assert_eq!(q.caption_tracks[0].captions[0].text, "Welcome to CADR");
    q.check().unwrap();

    // Single undo removes the imported caption track completely
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.active_sequence().unwrap().caption_tracks.is_empty());
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(s.active_sequence().unwrap().caption_tracks.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn silence_detect_preview_is_nondestructive_and_respects_threshold_and_padding() {
    // 0.0..1.0s tone (-6 dBFS), 1.0..2.5s pure silence (1.5s), 2.5..3.5s tone (-6 dBFS),
    // 3.5..4.8s quiet hum (-48 dBFS, amp ~0.0056), 4.8..6.0s tone (-6 dBFS)
    let segments = [
        (1.0, 0.5_f32),
        (1.5, 0.0_f32),
        (1.0, 0.5_f32),
        (1.3, 0.0056_f32), // RMS ~0.0040 -> ~-48 dBFS
        (1.2, 0.5_f32),
    ];
    let (mut s, v_id, _a_id, dir) = session_with_linked_wav("silence-preview", &segments);
    // Select ONLY the linked video clip to prove linked audio resolution works
    s.state.selection = vec![v_id];

    let rev_before = s.revision;
    let dur_before = s.active_sequence().unwrap().duration();
    let undo_before = s.history.undo.len();

    let d1 = s.execute("silence.detect", json!({"thresholdDb": -35.0, "minSilenceSeconds": 0.5, "paddingSeconds": 0.1})).unwrap();
    assert_eq!(d1["count"], 2, "both pure silence and -48 dB hum should be detected at -35 dB: {d1}");
    assert_eq!(s.revision, rev_before, "silence.detect must be non-destructive");
    assert_eq!(s.active_sequence().unwrap().duration(), dur_before);
    assert_eq!(s.history.undo.len(), undo_before);

    let r0_dur = d1["ranges"][0]["durationSec"].as_f64().unwrap();
    assert!((r0_dur - 1.3).abs() < 0.12, "expected ~1.3s cut (1.5s - 2*0.1s padding), got {r0_dur}");

    // Larger padding shrinks the cut duration cleanly
    let d_pad = s.execute("silence.detect", json!({"thresholdDb": -35.0, "minSilenceSeconds": 0.5, "paddingSeconds": 0.35})).unwrap();
    assert_eq!(d_pad["count"], 2);
    assert!(d_pad["totalSeconds"].as_f64().unwrap() < d1["totalSeconds"].as_f64().unwrap() - 0.8);

    // Stricter threshold (-60 dB) ignores the -48 dBFS quiet hum and detects only pure silence
    let d_strict = s.execute("silence.detect", json!({"thresholdDb": -60.0, "minSilenceSeconds": 0.5, "paddingSeconds": 0.1})).unwrap();
    assert_eq!(d_strict["count"], 1, "at -60 dB only pure digital silence should match: {d_strict}");

    // Min duration longer than any silent span -> 0 ranges
    let d_long = s.execute("silence.detect", json!({"thresholdDb": -35.0, "minSilenceSeconds": 2.0, "paddingSeconds": 0.1})).unwrap();
    assert_eq!(d_long["count"], 0);

    // Pure unit check on `detect_silence_in_samples`
    let direct = detect_silence_in_samples(
        &synth_samples(&[(0.5, 0.5), (1.0, 0.0), (0.5, 0.5)], ANALYSIS_SAMPLE_RATE),
        ANALYSIS_SAMPLE_RATE,
        Tick::ZERO,
        SilenceOptions { threshold_db: -35.0, min_silence_seconds: 0.4, padding_seconds: 0.1 },
        FrameRate::FPS_25,
    );
    assert_eq!(direct.len(), 1);
    let cut_sec = direct[0].duration.0 as f64 / TICKS_PER_SECOND as f64;
    assert!((cut_sec - 0.8).abs() < 0.1, "expected ~0.8s, got {cut_sec}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn silence_apply_preserves_av_sync_and_is_single_step_undo_safe() {
    // 1.0s tone, 1.5s silence, 1.0s tone, 1.5s silence, 1.0s tone (6.0s total)
    let segments = [(1.0, 0.5_f32), (1.5, 0.0_f32), (1.0, 0.5_f32), (1.5, 0.0_f32), (1.0, 0.5_f32)];
    let (mut s, v_id, _a_id, dir) = session_with_linked_wav("silence-apply", &segments);
    s.state.selection = vec![v_id];

    let orig_dur = s.active_sequence().unwrap().duration();
    let orig_v_count = s.active_sequence().unwrap().video_tracks[0].items.len();
    let orig_a_count = s.active_sequence().unwrap().audio_tracks[0].items.len();
    assert_eq!(orig_v_count, 1);
    assert_eq!(orig_a_count, 1);

    let preview = s.execute("silence.detect", json!({"thresholdDb": -35.0, "minSilenceSeconds": 0.5, "paddingSeconds": 0.1})).unwrap();
    assert_eq!(preview["count"], 2);
    let expected_cut_ticks = preview["totalTicks"].as_i64().unwrap();
    assert!(expected_cut_ticks > 0);

    let applied = s.execute("silence.apply", json!({"ranges": preview["ranges"]})).unwrap();
    assert_eq!(applied["removed"], 2);
    assert_eq!(applied["ticks"].as_i64().unwrap(), expected_cut_ticks);

    // Verify A/V sync across every remaining segment
    let q = s.active_sequence().unwrap();
    q.check().unwrap();
    assert_eq!(q.duration(), orig_dur - Tick(expected_cut_ticks));
    let v_items = &q.video_tracks[0].items;
    let a_items = &q.audio_tracks[0].items;
    assert_eq!(v_items.len(), 3, "two middle silence cuts leave 3 remaining segments");
    assert_eq!(v_items.len(), a_items.len(), "video and audio must have identical segment counts");
    for (vi, ai) in v_items.iter().zip(a_items.iter()) {
        assert_eq!(vi.start, ai.start, "A/V start must stay locked in sync");
        assert_eq!(vi.duration, ai.duration, "A/V duration must stay locked in sync");
        assert_eq!(vi.source_in, ai.source_in, "A/V source_in must stay locked in sync");
    }

    // Single undo restores the exact original sequence
    s.execute("edit.undo", json!({})).unwrap();
    let q_undo = s.active_sequence().unwrap();
    q_undo.check().unwrap();
    assert_eq!(q_undo.duration(), orig_dur);
    assert_eq!(q_undo.video_tracks[0].items.len(), 1);
    assert_eq!(q_undo.audio_tracks[0].items.len(), 1);

    // Redo reapplies cleanly
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(s.active_sequence().unwrap().duration(), orig_dur - Tick(expected_cut_ticks));

    let _ = std::fs::remove_dir_all(&dir);
}
