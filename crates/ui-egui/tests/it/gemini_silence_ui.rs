//! Headless UI integration tests for:
//! 1. Settings ▸ Media Analysis & Transcription: encrypted BYOK Gemini API key save/clear.
//! 2. Text ▸ Captions ▸ Gemini AI (BYOK): synthetic generate, Arabic filler removal, word-by-word
//!    toggle, editable SRT preview, translation, and native caption track import (undo-safe).
//! 3. Text ▸ Silence: synthetic WAV silence detection preview (non-destructive), adjustable
//!    threshold/padding, selective apply across linked A/V tracks with sync preserved, and undo.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_engine::gemini::{GeminiTransport, encode_wav_pcm16};
use filmcraft_engine::secret_store::GEMINI_API_KEY;
use filmcraft_engine::silence::ANALYSIS_SAMPLE_RATE;
use filmcraft_media::{Generator, MediaSource};
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, SequenceSettings, TrackKind};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct MockTransport {
    calls: Mutex<usize>,
    responses: Mutex<Vec<(u16, String)>>,
}

impl MockTransport {
    fn new(responses: Vec<(u16, String)>) -> Arc<Self> {
        Arc::new(Self { calls: Mutex::new(0), responses: Mutex::new(responses) })
    }

    fn call_count(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl GeminiTransport for MockTransport {
    fn post_json(&self, _url: &str, _api_key: &str, _body: &Value) -> Result<(u16, String), String> {
        *self.calls.lock().unwrap() += 1;
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() { Err("unexpected extra Gemini call in UI test".into()) } else { Ok(q.remove(0)) }
    }
}

fn gemini_ok(inner: &Value) -> (u16, String) {
    let text = serde_json::to_string(inner).unwrap();
    let env = json!({"candidates": [{"content": {"parts": [{"text": text}]}}]});
    (200, serde_json::to_string(&env).unwrap())
}

fn tmp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("cadr-ui-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Register the real Windows DPAPI protector, as the app shells do at startup. The registry is
/// process-global and set-once, so repeated calls are harmless.
#[cfg(target_os = "windows")]
fn register_real_dpapi() {
    filmcraft_engine::secret_store::register_dpapi_backend(filmcraft_platform::dpapi::protect, filmcraft_platform::dpapi::unprotect);
}

fn synth_samples(segments: &[(f64, f32)], sample_rate: u32) -> Vec<f32> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    for &(dur_s, amp) in segments {
        let count = (dur_s * f64::from(sample_rate)).round() as usize;
        for _ in 0..count {
            let t = idx as f64 / f64::from(sample_rate);
            out.push(if amp > 0.0 { amp * (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 } else { 0.0 });
            idx += 1;
        }
    }
    out
}

fn linked_wav_session(name: &str, segments: &[(f64, f32)]) -> (Session, PathBuf) {
    let dir = tmp_dir(name);
    let wav_path = dir.join("audio.wav");
    let wav = encode_wav_pcm16(&synth_samples(segments, ANALYSIS_SAMPLE_RATE), ANALYSIS_SAMPLE_RATE);
    std::fs::write(&wav_path, wav).unwrap();

    let mut s = Session::default();
    s.secrets.set_dir(dir.clone());
    let r = s.execute("file.import", json!({"paths": [wav_path.to_string_lossy()]})).unwrap();
    let audio_item = filmcraft_project::ItemId(r["items"][0].as_u64().unwrap());
    let rate = FrameRate::FPS_25;
    let dur = rate.snap(s.project.item(audio_item).unwrap().duration()).max(rate.frame_duration());
    let gen_kind = Generator::BlackVideo;
    let vsrc = filmcraft_media::generators::GeneratorSource::new(gen_kind.clone(), 640, 360, rate, dur);
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
    let video_item = p.add_item("Camera", Label::Iris, ItemKind::Media(Box::new(video_clip)), None);
    let seq_id = p.new_sequence("Seq", SequenceSettings { width: 640, height: 360, frame_rate: rate, ..Default::default() }, 1, 1, None);
    let range = TimeRange::new(Tick::ZERO, dur);
    let mut vi = p.make_track_item(video_item, TrackKind::Video, Tick::ZERO, range, rate).unwrap();
    let mut ai = p.make_track_item(audio_item, TrackKind::Audio, Tick::ZERO, range, rate).unwrap();
    let link = p.next_id;
    p.next_id += 1;
    vi.link = Some(link);
    ai.link = Some(link);
    let v_id = vi.id;
    let q = p.sequence_mut(seq_id).unwrap();
    q.video_tracks[0].items.push(vi);
    q.audio_tracks[0].items.push(ai);
    q.check().unwrap();
    s.project = Arc::new(p);
    s.state.active_sequence = Some(seq_id);
    s.state.selection = vec![v_id];
    (s, dir)
}

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
    fn from_session(session: Session) -> Self {
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx };
        d.frames(4);
        d
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
                return v["result"].clone();
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ids(&mut self, prefix: &str) -> Vec<String> {
        let v = self.ok("ui.elements", json!({"prefix": prefix}));
        v.as_array().unwrap().iter().filter_map(|e| e["id"].as_str().map(str::to_string)).collect()
    }
}

#[test]
fn settings_gemini_api_key_save_clear_and_respects_platform_storage() {
    #[cfg(target_os = "windows")]
    register_real_dpapi();
    let dir = tmp_dir("settings-key");
    let mut s = Session::default();
    s.secrets.set_dir(dir.clone());
    let mut d = Driver::from_session(s);

    d.ok("ui.menu.invoke", json!({"id": "app.settings.mediaAnalysis"}));
    d.frames(3);
    assert!(d.ids("settings.gemini.").contains(&"settings.gemini.apiKey".to_string()));

    let synthetic_key = "AIzaSy-UI-TEST-SECRET-4321";
    // Type into the password-masked API key input via egui input events
    d.ok("ui.click", json!({"id": "settings.gemini.apiKey"}));
    d.frames(2);
    d.harness.input_mut().events.push(egui::Event::Text(synthetic_key.into()));
    d.frames(2);

    d.ok("ui.click", json!({"id": "settings.gemini.saveKey"}));
    d.frames(3);
    assert!(d.harness.state().session.secrets.is_configured(GEMINI_API_KEY));
    assert_eq!(d.harness.state().session.secrets.get(GEMINI_API_KEY).unwrap().as_deref(), Some(synthetic_key));

    #[cfg(target_os = "windows")]
    {
        assert!(d.harness.state().session.secrets.is_persistent());
        let raw = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
        assert!(!raw.contains(synthetic_key), "plaintext API key leaked into secrets.json!");
    }

    #[cfg(not(target_os = "windows"))]
    {
        assert!(!d.harness.state().session.secrets.is_persistent());
        assert!(!dir.join("secrets.json").exists(), "secrets.json must not exist on session-only platforms");
        assert!(!dir.join(".secret.salt").exists(), ".secret.salt must not exist on session-only platforms");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no files may be written to data_dir on session-only platforms");
        let msg = d.harness.state().ui.settings.as_ref().unwrap().message.clone();
        assert!(msg.to_ascii_lowercase().contains("session only"), "Settings UI must indicate session-only storage, got: {msg}");
    }

    // Clear key via UI button
    d.ok("ui.click", json!({"id": "settings.gemini.clearKey"}));
    d.frames(3);
    assert!(!d.harness.state().session.secrets.is_configured(GEMINI_API_KEY));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Real Windows DPAPI persistence (moved from the engine test, whose binary has no OS backend).
#[cfg(target_os = "windows")]
#[test]
fn windows_dpapi_persists_gemini_key_encrypted_and_reloads() {
    register_real_dpapi();
    let dir = tmp_dir("dpapi-store");
    let secret = "AIzaSy-TEST-SYNTHETIC-KEY-9876543210";

    let mut store = filmcraft_engine::secret_store::SecretStore::for_dir(dir.clone());
    assert!(!store.is_configured(GEMINI_API_KEY));
    store.set(GEMINI_API_KEY, secret).unwrap();
    assert!(store.is_configured(GEMINI_API_KEY));
    assert_eq!(store.get(GEMINI_API_KEY).unwrap().as_deref(), Some(secret));

    assert!(store.is_persistent());
    // Reload from DPAPI-protected secrets.json in a fresh instance
    let store2 = filmcraft_engine::secret_store::SecretStore::for_dir(dir.clone());
    assert_eq!(store2.get(GEMINI_API_KEY).unwrap().as_deref(), Some(secret));
    let secrets_raw = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
    assert!(!secrets_raw.contains(secret), "plaintext secret leaked into secrets.json!");
    assert!(secrets_raw.contains("cadr.dpapi.v1:"), "expected DPAPI prefix in secrets.json");

    store.delete(GEMINI_API_KEY).unwrap();
    assert!(!store.is_configured(GEMINI_API_KEY));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gemini_captions_panel_generate_edit_translate_and_import() {
    let (mut s, dir) = linked_wav_session("gemini-ui", &[(2.5, 0.4)]);
    s.execute("gemini.setApiKey", json!({"key": "AIzaSy-MOCK-UI-KEY"})).unwrap();
    let transport = MockTransport::new(vec![
        gemini_ok(&json!({
            "lines": [
                { "text": "يعني أهلا بكم في كادر", "start": 0.1, "end": 1.2 },
                { "text": "إمم مونتاج سريع", "start": 1.3, "end": 2.3 }
            ],
            "words": [
                { "word": "يعني", "start": 0.1, "end": 0.3 },
                { "word": "أهلا", "start": 0.3, "end": 0.6 },
                { "word": "بكم", "start": 0.6, "end": 0.9 },
                { "word": "في", "start": 0.9, "end": 1.0 },
                { "word": "كادر", "start": 1.0, "end": 1.2 },
                { "word": "إمم", "start": 1.3, "end": 1.5 },
                { "word": "مونتاج", "start": 1.5, "end": 1.9 },
                { "word": "سريع", "start": 1.9, "end": 2.3 }
            ]
        })),
        gemini_ok(&json!({
            "translations": ["Welcome to CADR Studio", "Fast editing"]
        })),
    ]);
    s.gemini_transport = Some(transport.clone());

    let mut d = Driver::from_session(s);
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.tab.Captions"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "text.captions.gemini"}));
    d.frames(3);

    // Generate captions with default Arabic filler removal enabled
    d.ok("ui.click", json!({"id": "text.gemini.generate"}));
    d.frames(3);
    assert_eq!(transport.call_count(), 1);
    let srt = d.harness.state().ui.text_ai.srt_original.clone();
    assert!(srt.contains("أهلا بكم في كادر"), "expected cleaned Arabic caption: {srt}");
    assert!(!srt.contains("يعني"), "default Arabic filler should be stripped: {srt}");
    assert!(!srt.contains("إمم"), "default Arabic filler should be stripped: {srt}");

    // Toggle word-by-word: rebuilds SRT locally with zero extra API calls
    d.ok("ui.click", json!({"id": "text.gemini.wordByWord"}));
    d.frames(2);
    assert_eq!(transport.call_count(), 1, "toggling word-by-word must not spend an API call");
    let w_srt = d.harness.state().ui.text_ai.srt_original.clone();
    assert!(w_srt.contains("6\n"), "expected 6 non-filler word cues in word-by-word SRT: {w_srt}");

    // Toggle back to line captions and simulate user editing SRT preview before translating
    d.ok("ui.click", json!({"id": "text.gemini.wordByWord"}));
    d.frames(2);
    d.harness.state_mut().ui.text_ai.srt_original = d.harness.state().ui.text_ai.srt_original.replace("كادر", "كادر ستوديو");
    d.frames(2);

    // Translate preview
    d.ok("ui.click", json!({"id": "text.gemini.translate"}));
    d.frames(3);
    assert_eq!(transport.call_count(), 2);
    assert!(d.harness.state().ui.text_ai.show_translated);
    assert!(d.harness.state().ui.text_ai.srt_translated.contains("Welcome to CADR Studio"));

    // Import onto timeline -> creates native CaptionTrack and returns to Captions list
    d.ok("ui.click", json!({"id": "text.gemini.import"}));
    d.frames(3);
    assert!(!d.harness.state().ui.text_ai.gemini_open);
    let q = d.harness.state().session.active_sequence().unwrap();
    assert_eq!(q.caption_tracks.len(), 1);
    assert_eq!(q.caption_tracks[0].captions.len(), 2);
    assert_eq!(q.caption_tracks[0].captions[0].text, "Welcome to CADR Studio");

    // Undo removes imported track cleanly
    d.ok("engine.execute", json!({"command": "edit.undo", "params": {}}));
    assert!(d.harness.state().session.active_sequence().unwrap().caption_tracks.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn silence_tab_preview_adjust_apply_and_undo_with_av_sync() {
    // 1.0s tone, 1.5s silence, 1.0s tone, 1.5s silence, 1.0s tone (6.0s total)
    let segments = [(1.0, 0.5_f32), (1.5, 0.0_f32), (1.0, 0.5_f32), (1.5, 0.0_f32), (1.0, 0.5_f32)];
    let (s, dir) = linked_wav_session("silence-ui", &segments);
    let orig_dur = s.active_sequence().unwrap().duration();
    let orig_rev = s.revision;

    let mut d = Driver::from_session(s);
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.tab.Silence"}));
    d.frames(3);

    // Non-destructive preview detection
    d.ok("ui.click", json!({"id": "text.silence.detect"}));
    d.frames(3);
    assert_eq!(d.harness.state().ui.text_ai.silence_ranges.len(), 2);
    assert_eq!(d.harness.state().session.revision, orig_rev, "detect must be non-destructive");
    assert_eq!(d.harness.state().session.active_sequence().unwrap().duration(), orig_dur);
    let initial_cut_s: f64 = d.harness.state().ui.text_ai.silence_ranges.iter().map(|r| r.duration_sec).sum();

    // Adjust padding larger and re-detect -> smaller cut window
    d.harness.state_mut().ui.text_ai.padding_s = 0.30;
    d.frames(2);
    d.ok("ui.click", json!({"id": "text.silence.detect"}));
    d.frames(3);
    let padded_cut_s: f64 = d.harness.state().ui.text_ai.silence_ranges.iter().map(|r| r.duration_sec).sum();
    assert!(padded_cut_s < initial_cut_s - 0.5, "larger padding must shrink cut ranges: {padded_cut_s} vs {initial_cut_s}");

    // Uncheck the second silence range so only the first one is cut
    d.ok("ui.click", json!({"id": "text.silence.range.1"}));
    d.frames(2);
    assert!(d.harness.state().ui.text_ai.silence_ranges[0].included);
    assert!(!d.harness.state().ui.text_ai.silence_ranges[1].included);
    let cut0 = d.harness.state().ui.text_ai.silence_ranges[0].end - d.harness.state().ui.text_ai.silence_ranges[0].start;

    // Apply cut -> removes range 0 across linked video + audio in sync
    d.ok("ui.click", json!({"id": "text.silence.apply"}));
    d.frames(3);
    let q = d.harness.state().session.active_sequence().unwrap();
    q.check().unwrap();
    assert_eq!(q.duration(), orig_dur - Tick(cut0));
    assert_eq!(q.video_tracks[0].items.len(), 2);
    assert_eq!(q.audio_tracks[0].items.len(), 2);
    for (vi, ai) in q.video_tracks[0].items.iter().zip(&q.audio_tracks[0].items) {
        assert_eq!(vi.start, ai.start);
        assert_eq!(vi.duration, ai.duration);
    }

    // Single undo restores the original sequence
    d.ok("engine.execute", json!({"command": "edit.undo", "params": {}}));
    let q_undo = d.harness.state().session.active_sequence().unwrap();
    assert_eq!(q_undo.duration(), orig_dur);
    assert_eq!(q_undo.video_tracks[0].items.len(), 1);
    assert_eq!(q_undo.audio_tracks[0].items.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}
