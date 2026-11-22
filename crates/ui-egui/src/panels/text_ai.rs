//! Gemini BYOK Captions and Waveform Silence Cutting UI inside the Text panel.
//!
//! Reuses the native `captions.*` / `filmcraft_captions` and `ripple_delete_ranges` foundations via
//! engine commands (`gemini.*` and `silence.*`). Never stores or logs API keys.

use egui::{Color32, CornerRadius, Frame, Margin, Rect, RichText, Stroke, Ui, vec2};
use filmcraft_engine::gemini::{
    DEFAULT_ARABIC_FILLERS, GEMINI_MODELS, TRANSLATION_LANGUAGES, TimedLine, TimedWord, build_srt_from_lines, build_srt_from_words,
    remove_fillers_from_captions,
};
use filmcraft_engine::secret_store::GEMINI_API_KEY;
use filmcraft_engine::silence::{DEFAULT_MIN_SILENCE_SECONDS, DEFAULT_PADDING_SECONDS, DEFAULT_THRESHOLD_DB};
use filmcraft_time::{Tick, TimeDisplay, format_time};
use serde_json::json;

use crate::FilmcraftApp;
use crate::theme::Tokens;

const SOURCE_LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Auto-detect"),
    ("ar", "Arabic"),
    ("en", "English"),
    ("fr", "French"),
    ("es", "Spanish"),
    ("de", "German"),
    ("tr", "Turkish"),
    ("ru", "Russian"),
    ("hi", "Hindi"),
    ("ur", "Urdu"),
];

/// A single detected silence range shown in the non-destructive Silence preview list.
#[derive(Clone, Debug, PartialEq)]
pub struct SilencePreviewRange {
    pub start: i64,
    pub end: i64,
    pub start_sec: f64,
    pub end_sec: f64,
    pub duration_sec: f64,
    pub included: bool,
}

/// Transient UI state for Gemini BYOK Captions and Silence Cutting (held via `#[serde(skip)]` on
/// [`crate::state::UiState`] so project/UI JSON schemas stay backward compatible).
#[derive(Clone, Debug, PartialEq)]
pub struct TextAiState {
    /// Whether the Captions tab is currently showing the Gemini BYOK Captions workspace.
    pub gemini_open: bool,
    pub language: String,
    pub word_by_word: bool,
    pub remove_fillers: bool,
    pub custom_filler_input: String,
    pub raw_lines: Vec<TimedLine>,
    pub raw_words: Vec<TimedWord>,
    pub filler_hits: usize,
    pub srt_original: String,
    pub srt_translated: String,
    pub translation_lang: String,
    pub show_translated: bool,
    pub gemini_status: String,
    pub gemini_error: bool,

    // Silence cutting state (non-destructive preview until Apply is clicked)
    pub threshold_db: f64,
    pub min_silence_s: f64,
    pub padding_s: f64,
    pub silence_ranges: Vec<SilencePreviewRange>,
    pub silence_status: String,
    pub silence_error: bool,
}

impl Default for TextAiState {
    fn default() -> Self {
        Self {
            gemini_open: false,
            language: "auto".into(),
            word_by_word: false,
            remove_fillers: true,
            custom_filler_input: String::new(),
            raw_lines: Vec::new(),
            raw_words: Vec::new(),
            filler_hits: 0,
            srt_original: String::new(),
            srt_translated: String::new(),
            translation_lang: "English".into(),
            show_translated: false,
            gemini_status: String::new(),
            gemini_error: false,
            threshold_db: DEFAULT_THRESHOLD_DB,
            min_silence_s: DEFAULT_MIN_SILENCE_SECONDS,
            padding_s: DEFAULT_PADDING_SECONDS,
            silence_ranges: Vec::new(),
            silence_status: String::new(),
            silence_error: false,
        }
    }
}

impl TextAiState {
    /// Rebuild `srt_original` from cached `raw_lines` / `raw_words` when filler or word-by-word
    /// options change, without making another API request.
    pub fn rebuild_srt(&mut self, custom_fillers: &[String]) {
        if self.raw_lines.is_empty() && self.raw_words.is_empty() {
            return;
        }
        let (clean_lines, clean_words, hits) = remove_fillers_from_captions(&self.raw_lines, &self.raw_words, custom_fillers);
        self.filler_hits = hits.len();
        let (lines, words) =
            if self.remove_fillers { (clean_lines.as_slice(), clean_words.as_slice()) } else { (self.raw_lines.as_slice(), self.raw_words.as_slice()) };
        self.srt_original = if self.word_by_word { build_srt_from_words(words) } else { build_srt_from_lines(lines) };
    }
}

fn lang_display(code: &str) -> &'static str {
    SOURCE_LANGUAGES.iter().find(|(c, _)| *c == code).map_or("Auto-detect", |(_, label)| *label)
}

fn model_display(id: &str) -> String {
    GEMINI_MODELS.iter().find(|(m, _)| *m == id).map_or_else(|| id.to_string(), |(_, label)| (*label).to_string())
}

fn translation_iso(name: &str) -> &'static str {
    TRANSLATION_LANGUAGES.iter().find(|(n, _, _)| *n == name).map_or("en", |(_, iso, _)| *iso)
}

fn card_frame(t: &Tokens) -> Frame {
    Frame::new().fill(t.app_bg).stroke(Stroke::new(1.0, t.separator)).corner_radius(CornerRadius::same(4)).inner_margin(Margin::same(8))
}

/// Render the Gemini BYOK Captions panel inside the Text panel's Captions tab.
pub fn show_gemini_captions(app: &mut FilmcraftApp, ui: &mut Ui, rect: Rect) {
    let t = app.tokens;
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 4.0))).id_salt("text-gemini-root"));

    egui::ScrollArea::vertical().id_salt("text-gemini-scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        ui.set_width((rect.width() - 24.0).max(200.0));
        ui.spacing_mut().item_spacing.y = 6.0;

        // ---- Header row
        let configured = app.session.secrets.is_configured(GEMINI_API_KEY);
        let mut go_back = false;
        let mut open_settings = false;
        ui.horizontal_wrapped(|ui| {
            let back = ui.button(tl!("← Captions"));
            app.auto.add("text.gemini.back", back.rect, "Back to Captions");
            if back.clicked() {
                go_back = true;
            }
            ui.label(RichText::new(tl!("Gemini Captions (BYOK)")).size(13.0).strong().color(t.text));
            let key_label = if configured { tl!("API Key: Configured") } else { tl!("Set API Key…") };
            let key_btn = ui.button(key_label);
            app.auto.add("text.gemini.openSettings", key_btn.rect, "Gemini API Key Settings");
            if key_btn.clicked() {
                open_settings = true;
            }
        });
        if go_back {
            app.ui.text_ai.gemini_open = false;
            return;
        }
        if open_settings {
            let _ = super::settings::open(app, "mediaAnalysis");
        }

        if !configured {
            let notice = if app.session.secrets.is_persistent() {
                tl!("Bring your own Gemini API key in Settings ▸ Media Analysis & Transcription. Your key is protected by the OS credential store and never logged.")
            } else {
                tl!("Bring your own Gemini API key in Settings ▸ Media Analysis & Transcription. On this platform, your key is kept for this session only (in memory, never written to disk) and never logged.")
            };
            card_frame(&t).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(notice).size(11.5).color(t.text_dim)).wrap());
            });
        }

        // ---- Model, language & mode controls
        let mut new_model: Option<String> = None;
        let mut rebuild_needed = false;
        let mut generate_clicked = false;
        let current_model = app.session.prefs.media_analysis.gemini_model.clone();
        let mut custom_fillers = app.session.prefs.media_analysis.custom_fillers.clone();
        let mut fillers_changed = false;

        card_frame(&t).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tl!("Model")).size(11.5).color(t.text_dim));
                let model_label = model_display(&current_model);
                let resp = egui::ComboBox::from_id_salt("text-gemini-model")
                    .selected_text(&model_label)
                    .width(210.0)
                    .truncate()
                    .show_ui(ui, |ui| {
                        for &(id, label) in GEMINI_MODELS {
                            if ui.selectable_label(current_model == id, label).clicked() {
                                new_model = Some(id.to_string());
                            }
                        }
                    });
                let model_resp = resp.response.on_hover_text(&model_label);
                app.auto.add("text.gemini.model", model_resp.rect, "Gemini model");

                ui.label(RichText::new(tl!("Language")).size(11.5).color(t.text_dim));
                let cur_lang = app.ui.text_ai.language.clone();
                let lresp = egui::ComboBox::from_id_salt("text-gemini-language")
                    .selected_text(crate::i18n::t(lang_display(&cur_lang)))
                    .width(115.0)
                    .show_ui(ui, |ui| {
                        for &(code, label) in SOURCE_LANGUAGES {
                            if ui.selectable_label(cur_lang == code, crate::i18n::t(label)).clicked() {
                                app.ui.text_ai.language = code.to_string();
                            }
                        }
                    });
                app.auto.add("text.gemini.language", lresp.response.rect, "Caption language");
            });

            ui.horizontal_wrapped(|ui| {
                let wb = ui.checkbox(&mut app.ui.text_ai.word_by_word, tl!("Word-by-word captions"));
                app.auto.add("text.gemini.wordByWord", wb.rect, "Word-by-word captions");
                if wb.changed() {
                    rebuild_needed = true;
                }

                let rf = ui.checkbox(&mut app.ui.text_ai.remove_fillers, tl!("Remove filler words"));
                app.auto.add("text.gemini.removeFillers", rf.rect, "Remove filler words");
                if rf.changed() {
                    rebuild_needed = true;
                }
            });

            if app.ui.text_ai.remove_fillers {
                ui.add(
                    egui::Label::new(
                        RichText::new(tlf!("Default Arabic fillers: {list}", list = DEFAULT_ARABIC_FILLERS.join(" · ")))
                            .size(11.0)
                            .color(t.text_dim),
                    )
                    .wrap(),
                );
                ui.horizontal_wrapped(|ui| {
                    let inp = crate::widgets::accessible_name(
                        ui.add(
                            egui::TextEdit::singleline(&mut app.ui.text_ai.custom_filler_input)
                                .desired_width(140.0)
                                .hint_text(tl!("Custom filler word")),
                        ),
                        "Custom filler word",
                    );
                    app.auto.add("text.gemini.customFiller", inp.rect, "Custom filler word");
                    let add_btn = ui.button(tl!("Add"));
                    app.auto.add("text.gemini.addFiller", add_btn.rect, "Add custom filler");
                    let submit = add_btn.clicked() || (inp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if submit {
                        let w = app.ui.text_ai.custom_filler_input.trim().to_string();
                        if !w.is_empty() && !custom_fillers.iter().any(|x| x == &w) {
                            custom_fillers.push(w);
                            fillers_changed = true;
                            rebuild_needed = true;
                        }
                        app.ui.text_ai.custom_filler_input.clear();
                    }
                });
                if !custom_fillers.is_empty() {
                    let mut remove_idx: Option<usize> = None;
                    ui.horizontal_wrapped(|ui| {
                        for (i, f) in custom_fillers.iter().enumerate() {
                            let b = ui.small_button(format!("{f} ×"));
                            app.auto.add(&format!("text.gemini.removeFiller.{i}"), b.rect, f);
                            if b.clicked() {
                                remove_idx = Some(i);
                            }
                        }
                    });
                    if let Some(i) = remove_idx {
                        custom_fillers.remove(i);
                        fillers_changed = true;
                        rebuild_needed = true;
                    }
                }
            }

            ui.horizontal_wrapped(|ui| {
                let gen_btn = ui.add_enabled(
                    configured,
                    egui::Button::new(RichText::new(tl!("Generate Captions")).color(Color32::WHITE)).fill(if configured { t.accent } else { t.field_bg }),
                );
                app.auto.add("text.gemini.generate", gen_btn.rect, "Generate Captions");
                if gen_btn.clicked() {
                    generate_clicked = true;
                }
                if app.ui.text_ai.filler_hits > 0 {
                    ui.label(
                        RichText::new(tlf!("{count} filler hit(s) detected", count = app.ui.text_ai.filler_hits))
                            .size(11.0)
                            .color(t.text_dim),
                    );
                }
            });
        });

        if let Some(m) = new_model {
            let _ = app.session.execute("prefs.set", json!({"key": "mediaAnalysis.geminiModel", "value": m}));
        }
        if fillers_changed {
            let _ = app.session.execute("prefs.set", json!({"key": "mediaAnalysis.customFillers", "value": custom_fillers}));
        }
        if rebuild_needed {
            app.ui.text_ai.rebuild_srt(&app.session.prefs.media_analysis.custom_fillers);
        }

        if generate_clicked {
            let params = json!({
                "model": app.session.prefs.media_analysis.gemini_model,
                "language": app.ui.text_ai.language,
                "wordByWord": app.ui.text_ai.word_by_word,
                "removeFillers": app.ui.text_ai.remove_fillers,
                "customFillers": app.session.prefs.media_analysis.custom_fillers,
            });
            match app.session.execute("gemini.generateCaptions", params) {
                Ok(v) => {
                    let raw_lines: Vec<TimedLine> = serde_json::from_value(v.get("rawLines").cloned().unwrap_or_else(|| v["lines"].clone())).unwrap_or_default();
                    let raw_words: Vec<TimedWord> = serde_json::from_value(v.get("rawWords").cloned().unwrap_or_else(|| v["words"].clone())).unwrap_or_default();
                    let hits = v["fillerHits"].as_array().map_or(0, Vec::len);
                    let srt = v["srt"].as_str().unwrap_or("").to_string();
                    let line_count = v["lines"].as_array().map_or(0, Vec::len);
                    let word_count = v["words"].as_array().map_or(0, Vec::len);
                    app.ui.text_ai.raw_lines = raw_lines;
                    app.ui.text_ai.raw_words = raw_words;
                    app.ui.text_ai.filler_hits = hits;
                    app.ui.text_ai.srt_original = srt;
                    app.ui.text_ai.srt_translated.clear();
                    app.ui.text_ai.show_translated = false;
                    app.ui.text_ai.gemini_error = false;
                    app.ui.text_ai.gemini_status = tlf!("Generated {lines} lines ({words} words). Edit SRT below before importing.", lines = line_count, words = word_count);
                }
                Err(e) => {
                    app.ui.text_ai.gemini_error = true;
                    app.ui.text_ai.gemini_status = e.to_string();
                }
            }
        }

        // ---- Editable SRT Preview + Translation + Import
        card_frame(&t).show(ui, |ui| {
            let has_trans = !app.ui.text_ai.srt_translated.is_empty();
            let has_srt = !app.ui.text_ai.srt_original.trim().is_empty();
            let mut translate_clicked = false;
            let mut import_clicked = false;

            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tl!("SRT Preview (editable)")).size(12.0).strong().color(t.text));
                if has_trans {
                    let o = ui.selectable_label(!app.ui.text_ai.show_translated, tl!("Original"));
                    app.auto.add("text.gemini.viewOriginal", o.rect, "Original SRT");
                    if o.clicked() {
                        app.ui.text_ai.show_translated = false;
                    }
                    let tr = ui.selectable_label(app.ui.text_ai.show_translated, tl!("Translation"));
                    app.auto.add("text.gemini.viewTranslated", tr.rect, "Translated SRT");
                    if tr.clicked() {
                        app.ui.text_ai.show_translated = true;
                    }
                }
            });

            // Translation & Import action bar (always visible without scrolling past SRT body)
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tl!("Translate to")).size(11.5).color(t.text_dim));
                let cur_tl = app.ui.text_ai.translation_lang.clone();
                let tr_combo = egui::ComboBox::from_id_salt("text-gemini-translate-lang")
                    .selected_text(&cur_tl)
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for &(name, _, label) in TRANSLATION_LANGUAGES {
                            if ui.selectable_label(cur_tl == name, label).clicked() {
                                app.ui.text_ai.translation_lang = name.to_string();
                            }
                        }
                    });
                app.auto.add("text.gemini.translateLang", tr_combo.response.rect, "Translation language");

                let tr_btn = ui.add_enabled(configured && has_srt, egui::Button::new(tl!("Translate")));
                app.auto.add("text.gemini.translate", tr_btn.rect, "Translate SRT");
                if tr_btn.clicked() {
                    translate_clicked = true;
                }

                let active_nonempty = if app.ui.text_ai.show_translated && has_trans {
                    !app.ui.text_ai.srt_translated.trim().is_empty()
                } else {
                    has_srt
                };
                let imp_btn = ui.add_enabled(
                    active_nonempty,
                    egui::Button::new(RichText::new(tl!("Import to Timeline")).color(Color32::WHITE))
                        .fill(if active_nonempty { t.accent } else { t.field_bg }),
                );
                app.auto.add("text.gemini.import", imp_btn.rect, "Import SRT to Timeline");
                if imp_btn.clicked() {
                    import_clicked = true;
                }
            });

            let active_buf = if app.ui.text_ai.show_translated && has_trans {
                &mut app.ui.text_ai.srt_translated
            } else {
                &mut app.ui.text_ai.srt_original
            };
            let preview_w = (ui.available_width() - 4.0).max(160.0);
            let te = crate::widgets::accessible_name(
                ui.add(
                    egui::TextEdit::multiline(active_buf)
                        .desired_width(preview_w)
                        .desired_rows(6)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(tl!("Generated SRT captions will appear here. You can edit text or timings before importing onto the timeline.")),
                ),
                "SRT Preview",
            );
            app.auto.add("text.gemini.srtPreview", te.rect, "SRT Preview");

            if translate_clicked {
                let params = json!({
                    "srt": app.ui.text_ai.srt_original,
                    "targetLanguage": app.ui.text_ai.translation_lang,
                    "model": app.session.prefs.media_analysis.gemini_model,
                });
                match app.session.execute("gemini.translateSrt", params) {
                    Ok(v) => {
                        app.ui.text_ai.srt_translated = v["srt"].as_str().unwrap_or("").to_string();
                        app.ui.text_ai.show_translated = true;
                        app.ui.text_ai.gemini_error = false;
                        let cues = v["cues"].as_u64().unwrap_or(0);
                        let lang = app.ui.text_ai.translation_lang.clone();
                        app.ui.text_ai.gemini_status = tlf!("Translated {cues} cues to {lang}.", cues, lang);
                    }
                    Err(e) => {
                        app.ui.text_ai.gemini_error = true;
                        app.ui.text_ai.gemini_status = e.to_string();
                    }
                }
            }

            if import_clicked {
                let use_trans = app.ui.text_ai.show_translated && !app.ui.text_ai.srt_translated.trim().is_empty();
                let srt_payload = if use_trans { app.ui.text_ai.srt_translated.clone() } else { app.ui.text_ai.srt_original.clone() };
                let track_name = if use_trans {
                    format!("Gemini ({})", app.ui.text_ai.translation_lang)
                } else {
                    "Gemini Captions".to_string()
                };
                let lang_code = if use_trans {
                    translation_iso(&app.ui.text_ai.translation_lang).to_string()
                } else {
                    app.ui.text_ai.language.clone()
                };
                match app.session.execute("gemini.importSrt", json!({"srt": srt_payload, "name": track_name, "language": lang_code})) {
                    Ok(v) => {
                        let cues = v["captions"].as_u64().unwrap_or(0);
                        app.ui.text_ai.gemini_error = false;
                        app.ui.text_ai.gemini_status = tlf!("Imported {cues} caption(s) onto timeline.", cues);
                        app.ui.text_ai.gemini_open = false;
                    }
                    Err(e) => {
                        app.ui.text_ai.gemini_error = true;
                        app.ui.text_ai.gemini_status = e.to_string();
                    }
                }
            }
        });

        if !app.ui.text_ai.gemini_status.is_empty() {
            let color = if app.ui.text_ai.gemini_error { t.danger } else { t.text_dim };
            ui.add(egui::Label::new(RichText::new(&app.ui.text_ai.gemini_status).size(11.5).color(color)).wrap());
        }
    });
}

/// Render the Silence Cutting tab inside the Text panel.
pub fn show_silence_tab(app: &mut FilmcraftApp, ui: &mut Ui, rect: Rect) {
    let t = app.tokens;
    let project = app.session.project.clone();
    let Some(seq) = app.session.state.active_sequence.and_then(|id| project.sequence(id)) else {
        crate::dock::placeholder(ui, rect, &t, tl!("Open a sequence to detect and cut silences"));
        return;
    };
    let rate = seq.settings.frame_rate;
    let df = seq.settings.drop_frame;
    let tc = |tick: i64| format_time(Tick(tick), rate, df, TimeDisplay::Timecode, 48_000);

    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 4.0))).id_salt("text-silence-root"));
    egui::ScrollArea::vertical().id_salt("text-silence-scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        ui.set_width((rect.width() - 24.0).max(200.0));
        ui.spacing_mut().item_spacing.y = 6.0;

        ui.label(RichText::new(tl!("Silence Cutting")).size(13.0).strong().color(t.text));
        ui.add(
            egui::Label::new(
                RichText::new(tl!(
                    "Detect quiet ranges in selected audio (or the active sequence), preview them non-destructively, then remove them across audio and video in one undoable step."
                ))
                .size(11.5)
                .color(t.text_dim),
            )
            .wrap(),
        );

        let mut detect_clicked = false;
        let mut apply_clicked = false;
        let mut clear_clicked = false;

        card_frame(&t).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tl!("Threshold")).size(11.5).color(t.text_dim));
                let th = ui.add(
                    crate::widgets::number_drag(&mut app.ui.text_ai.threshold_db)
                        .range(-90.0..=-1.0)
                        .speed(0.5)
                        .fixed_decimals(1)
                        .suffix(" dB"),
                );
                app.auto.add("text.silence.threshold", th.rect, "Silence threshold dB");

                ui.label(RichText::new(tl!("Min silence")).size(11.5).color(t.text_dim));
                let md = ui.add(
                    crate::widgets::number_drag(&mut app.ui.text_ai.min_silence_s)
                        .range(0.05..=30.0)
                        .speed(0.05)
                        .fixed_decimals(2)
                        .suffix(" s"),
                );
                app.auto.add("text.silence.minDuration", md.rect, "Minimum silence duration");

                ui.label(RichText::new(tl!("Padding")).size(11.5).color(t.text_dim));
                let pd = ui.add(
                    crate::widgets::number_drag(&mut app.ui.text_ai.padding_s)
                        .range(0.0..=5.0)
                        .speed(0.01)
                        .fixed_decimals(2)
                        .suffix(" s"),
                );
                app.auto.add("text.silence.padding", pd.rect, "Silence cut padding");
            });

            let selected_count = app.ui.text_ai.silence_ranges.iter().filter(|r| r.included).count();
            ui.horizontal_wrapped(|ui| {
                let d_btn = ui.button(tl!("Detect Silences"));
                app.auto.add("text.silence.detect", d_btn.rect, "Detect Silences");
                if d_btn.clicked() {
                    detect_clicked = true;
                }

                let a_btn = ui.add_enabled(
                    selected_count > 0,
                    egui::Button::new(RichText::new(tl!("Apply Cut")).color(Color32::WHITE))
                        .fill(if selected_count > 0 { t.accent } else { t.field_bg }),
                );
                app.auto.add("text.silence.apply", a_btn.rect, "Apply Silence Cut");
                if a_btn.clicked() {
                    apply_clicked = true;
                }

                let c_btn = ui.add_enabled(!app.ui.text_ai.silence_ranges.is_empty(), egui::Button::new(tl!("Clear Preview")));
                app.auto.add("text.silence.clear", c_btn.rect, "Clear Silence Preview");
                if c_btn.clicked() {
                    clear_clicked = true;
                }
            });
        });

        if detect_clicked {
            let params = json!({
                "thresholdDb": app.ui.text_ai.threshold_db,
                "minSilenceSeconds": app.ui.text_ai.min_silence_s,
                "paddingSeconds": app.ui.text_ai.padding_s,
            });
            match app.session.execute("silence.detect", params) {
                Ok(v) => {
                    let mut list = Vec::new();
                    if let Some(arr) = v["ranges"].as_array() {
                        for item in arr {
                            let start = item["start"].as_i64().unwrap_or(0);
                            let end = item["end"].as_i64().unwrap_or(0);
                            if end > start {
                                list.push(SilencePreviewRange {
                                    start,
                                    end,
                                    start_sec: item["startSec"].as_f64().unwrap_or(0.0),
                                    end_sec: item["endSec"].as_f64().unwrap_or(0.0),
                                    duration_sec: item["durationSec"].as_f64().unwrap_or(0.0),
                                    included: true,
                                });
                            }
                        }
                    }
                    let total_s = v["totalSeconds"].as_f64().unwrap_or(0.0);
                    let count = list.len();
                    app.ui.text_ai.silence_ranges = list;
                    app.ui.text_ai.silence_error = false;
                    app.ui.text_ai.silence_status = if count == 0 {
                        tl!("No silent ranges found at these settings.").into()
                    } else {
                        tlf!(
                            "Preview: {count} silent range(s) ({secs} s total). Non-destructive until applied.",
                            count = count,
                            secs = format!("{total_s:.2}")
                        )
                    };
                }
                Err(e) => {
                    app.ui.text_ai.silence_error = true;
                    app.ui.text_ai.silence_status = e.to_string();
                }
            }
        }

        if clear_clicked {
            app.ui.text_ai.silence_ranges.clear();
            app.ui.text_ai.silence_error = false;
            app.ui.text_ai.silence_status.clear();
        }

        if apply_clicked {
            let chosen: Vec<serde_json::Value> = app
                .ui
                .text_ai
                .silence_ranges
                .iter()
                .filter(|r| r.included)
                .map(|r| json!({"start": r.start, "end": r.end}))
                .collect();
            match app.session.execute("silence.apply", json!({"ranges": chosen})) {
                Ok(v) => {
                    let removed = v["removed"].as_u64().unwrap_or(0);
                    let secs = v["seconds"].as_f64().unwrap_or(0.0);
                    app.ui.text_ai.silence_ranges.clear();
                    app.ui.text_ai.silence_error = false;
                    app.ui.text_ai.silence_status = tlf!(
                        "Removed {removed} silent range(s) ({secs} s) with A/V sync preserved. Undo anytime.",
                        removed = removed,
                        secs = format!("{secs:.2}")
                    );
                }
                Err(e) => {
                    app.ui.text_ai.silence_error = true;
                    app.ui.text_ai.silence_status = e.to_string();
                }
            }
        }

        if !app.ui.text_ai.silence_status.is_empty() {
            let color = if app.ui.text_ai.silence_error { t.danger } else { t.text_dim };
            ui.add(egui::Label::new(RichText::new(&app.ui.text_ai.silence_status).size(11.5).color(color)).wrap());
        }

        // ---- Preview range list
        if !app.ui.text_ai.silence_ranges.is_empty() {
            let mut jump_to: Option<i64> = None;
            card_frame(&t).show(ui, |ui| {
                let checked_secs: f64 = app.ui.text_ai.silence_ranges.iter().filter(|r| r.included).map(|r| r.duration_sec).sum();
                let checked_cnt = app.ui.text_ai.silence_ranges.iter().filter(|r| r.included).count();
                ui.label(
                    RichText::new(tlf!(
                        "Detected Silences ({checked}/{total} selected · {secs} s)",
                        checked = checked_cnt,
                        total = app.ui.text_ai.silence_ranges.len(),
                        secs = format!("{checked_secs:.2}")
                    ))
                    .size(11.5)
                    .strong()
                    .color(t.text),
                );
                for (i, rng) in app.ui.text_ai.silence_ranges.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let cb = ui.checkbox(&mut rng.included, "");
                        app.auto.add(&format!("text.silence.range.{i}"), cb.rect, "Include silent range");
                        let label = format!("{} → {}  ({:.2} s)", tc(rng.start), tc(rng.end), rng.duration_sec);
                        let jb = ui.selectable_label(false, RichText::new(&label).monospace().size(11.5).color(t.timecode));
                        app.auto.add(&format!("text.silence.jump.{i}"), jb.rect, &label);
                        if jb.clicked() {
                            jump_to = Some(rng.start);
                        }
                    });
                }
            });
            if let Some(tick) = jump_to {
                let _ = app.session.execute("playback.seek", json!({"time": tick}));
            }
        }
    });
}
