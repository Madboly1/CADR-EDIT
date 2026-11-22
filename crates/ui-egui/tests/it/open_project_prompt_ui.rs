//! Headless UI tests of File ▸ Open Project, File ▸ Open Recent and Home's Recent list with unsaved
//! changes: once the project to open is chosen, the Save Project prompt asks Cancel / Don't Save /
//! Save before it opens.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_engine::project::{ClipId, Label};
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use filmcraft_ui_egui::native_dialogs::FileDialog;
use filmcraft_ui_egui::state::Mode;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
    fn new() -> Self {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(s).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx };
        d.frames(4);
        d
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            // the control channel's clicks and keys enter through the input hook
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let v = self.call(method, params.clone());
        assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
        v["result"].clone()
    }

    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }

    fn menu(&mut self, id: &str) -> Value {
        let r = self.ok("ui.menu.invoke", json!({"id": id}));
        self.frames(3);
        r
    }

    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id": id}));
        self.frames(3);
    }

    fn has(&mut self, id: &str) -> bool {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        v.as_array().unwrap().iter().any(|e| e["id"] == json!(id))
    }

    fn app(&mut self) -> &mut FilmcraftApp {
        self.harness.state_mut()
    }

    fn path(&mut self) -> Option<String> {
        self.app().session.path.clone()
    }

    /// Make an unsaved change: the first clip turns yellow. Returns the clip.
    fn edit(&mut self) -> ClipId {
        let c = self.app().session.active_sequence().unwrap().video_tracks[0].items[0].id;
        self.exec("timeline.select", json!({"clips": [c.0]}));
        self.menu("edit.label.yellow");
        assert!(self.app().session.is_dirty());
        c
    }

    /// The label of `clip` in the open project.
    fn label(&mut self, clip: ClipId) -> Label {
        let p = &self.app().session.project;
        p.sequences().filter_map(|i| p.sequence(i.id)).find_map(|q| q.find_item(clip).map(|(_, it)| it.label)).unwrap()
    }

    /// File ▸ Open Project… with the Open dialog answering `path`.
    fn open_project(&mut self, path: &str) -> Value {
        let path = path.to_string();
        self.app().hooks.pick_open_project = Some(Box::new(move || Some(path.clone())));
        self.menu("file.open")
    }

    /// Where `path` is in the recent projects (File ▸ Open Recent, Home's Recent list).
    fn recent(&mut self, path: &str) -> usize {
        self.app().session.prefs.general.recent_projects.iter().position(|p| p == path).unwrap()
    }
}

/// A folder for one test holding the demo project saved as `current.fcproj` and `other.fcproj`.
/// The session is left on `current.fcproj` with nothing unsaved.
fn projects(d: &mut Driver, test: &str) -> (PathBuf, String, String) {
    let dir = std::env::temp_dir().join(format!("fc-open-project-prompt-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let current = dir.join("current.fcproj").to_string_lossy().into_owned();
    let other = dir.join("other.fcproj").to_string_lossy().into_owned();
    d.exec("file.saveAs", json!({"path": other}));
    d.exec("file.saveAs", json!({"path": current}));
    assert!(!d.app().session.is_dirty());
    (dir, current, other)
}

#[test]
fn open_project_save_saves_the_changes_then_opens_the_chosen_project() {
    let mut d = Driver::new();
    let (dir, current, other) = projects(&mut d, "save");
    let clip = d.edit();
    d.open_project(&other);
    d.click("openProject.save");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.path(), Some(other));
    assert!(!d.app().session.is_dirty());
    // the change was written to the project it was made in
    d.exec("file.open", json!({"path": current}));
    assert_eq!(d.label(clip), Label::Yellow);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_project_failed_save_keeps_the_prompt_and_chosen_project() {
    let mut d = Driver::new();
    let (dir, _, other) = projects(&mut d, "failedSave");
    let folder = dir.join("current-folder");
    std::fs::create_dir(&folder).unwrap();
    let current = folder.join("current.fcproj").to_string_lossy().into_owned();
    d.exec("file.saveAs", json!({"path": current}));
    let clip = d.edit();
    std::fs::remove_dir_all(&folder).unwrap();
    std::fs::write(&folder, b"not a folder").unwrap();

    d.open_project(&other);
    d.click("openProject.save");
    assert!(d.has("openProject.save"), "the prompt stays open after a failed save");
    assert_eq!(d.app().ui.clip_dialog.as_ref().unwrap().params["path"], json!(other));
    assert_eq!(d.path(), Some(current.clone()));
    assert!(d.app().session.is_dirty());
    assert_eq!(d.label(clip), Label::Yellow);
    assert!(d.app().ui.status.contains(&current), "the status shows the save error: {}", d.app().ui.status);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn close_project_and_quit_failed_save_keep_the_prompt() {
    for (command, prompt) in [("file.closeProject", "closeProject"), ("app.quit", "quit")] {
        let mut d = Driver::new();
        let (dir, _, _) = projects(&mut d, prompt);
        let folder = dir.join("current-folder");
        std::fs::create_dir(&folder).unwrap();
        let current = folder.join("current.fcproj").to_string_lossy().into_owned();
        d.exec("file.saveAs", json!({"path": current}));
        let clip = d.edit();
        std::fs::remove_dir_all(&folder).unwrap();
        std::fs::write(&folder, b"not a folder").unwrap();

        if command == "app.quit" {
            d.harness.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
            d.frames(3);
        } else {
            d.menu(command);
        }
        let save = format!("{prompt}.save");
        assert!(d.has(&save), "{command} shows its save prompt");
        d.click(&save);
        assert!(d.has(&save), "{command} keeps its prompt after a failed save");
        assert_eq!(d.path(), Some(current.clone()));
        assert!(d.app().session.is_dirty());
        assert_eq!(d.label(clip), Label::Yellow, "{command} keeps the change");
        assert!(d.app().ui.status.contains(&current), "{command} shows the save error: {}", d.app().ui.status);
        assert!(!d.app().quit_confirmed, "{command} does not confirm a quit");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn home_recent_with_unsaved_changes_asks_first() {
    let mut d = Driver::new();
    let (dir, current, other) = projects(&mut d, "homeRecent");
    let first = d.app().session.active_sequence().unwrap().video_tracks[0].items[0].clone();
    let clip = d.edit();
    d.click("header.home");
    let i = d.recent(&other);
    d.click(&format!("import.recent.{i}"));
    assert_eq!(d.path(), Some(current.clone()), "nothing opens before an answer");
    assert_eq!(d.app().ui.mode, Mode::Import, "Home stays while it asks");
    // Cancel: Home stays, with this project and its changes
    d.click("openProject.cancel");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!((d.path(), d.app().ui.mode), (Some(current.clone()), Mode::Import));
    assert!(d.app().session.is_dirty());
    d.click(&format!("import.recent.{i}"));
    assert!(d.has("openProject.dontSave"));
    d.click("openProject.dontSave");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.path(), Some(other));
    assert_eq!(d.app().ui.mode, Mode::Edit);
    assert!(!d.app().session.is_dirty());
    // the project the change was made in is as it was saved
    d.exec("file.open", json!({"path": current}));
    assert_eq!(d.label(clip), first.label);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn home_recent_save_on_a_never_saved_project_switches_to_edit_once_it_opens() {
    let mut d = Driver::new();
    let (dir, _, other) = projects(&mut d, "homeNeverSaved");
    d.app().session.path = None;
    d.edit();
    let saved = dir.join("saved.fcproj").to_string_lossy().into_owned();
    d.app().hooks.pick_save = Some(Box::new(move |_| Some(saved.clone())));
    d.click("header.home");
    let i = d.recent(&other);
    d.click(&format!("import.recent.{i}"));
    assert_eq!(d.app().ui.mode, Mode::Import);
    d.click("openProject.save");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.path(), Some(other));
    assert_eq!(d.app().ui.mode, Mode::Edit);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_project_asks_when_the_open_dialog_answers_in_a_later_frame() {
    let mut d = Driver::new();
    let (dir, current, other) = projects(&mut d, "runner");
    d.edit();
    // a host that shows file dialogs without blocking (the desktop app on Linux)
    let answer: Arc<Mutex<Option<Sender<Vec<String>>>>> = Arc::default();
    let a = answer.clone();
    d.app().hooks.run_dialog = Some(Box::new(move |_| {
        let (tx, rx) = channel();
        *a.lock().unwrap() = Some(tx);
        rx
    }));
    assert_eq!(d.menu("file.open"), json!({"dialog": "open"}));
    answer.lock().unwrap().take().unwrap().send(vec![other.clone()]).unwrap();
    d.frames(3);
    assert!(d.has("openProject.dontSave"));
    assert_eq!(d.path(), Some(current));
    d.click("openProject.dontSave");
    assert_eq!(d.path(), Some(other));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_project_save_waits_for_a_save_dialog_answered_later() {
    let mut d = Driver::new();
    let (dir, _, other) = projects(&mut d, "saveLater");
    d.app().session.path = None;
    let clip = d.edit();
    let saved = dir.join("saved.fcproj").to_string_lossy().into_owned();
    // a host that shows file dialogs without blocking (the desktop app on Linux), answered by hand
    let asked: Arc<Mutex<Vec<(FileDialog, Sender<Vec<String>>)>>> = Arc::default();
    let a = asked.clone();
    d.app().hooks.run_dialog = Some(Box::new(move |dialog| {
        let (tx, rx) = channel();
        a.lock().unwrap().push((dialog, tx));
        rx
    }));
    let answer = |d: &mut Driver, paths: Vec<String>| {
        let (dialog, tx) = asked.lock().unwrap().remove(0);
        tx.send(paths).unwrap();
        d.frames(3);
        dialog
    };
    d.menu("file.open");
    assert!(matches!(answer(&mut d, vec![other.clone()]), FileDialog::OpenProject));
    // the Save dialog is canceled: back to the prompt, nothing opens
    d.click("openProject.save");
    assert!(matches!(answer(&mut d, Vec::new()), FileDialog::SaveProject { .. }));
    assert!(d.has("openProject.save"));
    assert_eq!(d.path(), None);
    // the save fails: the prompt stays, with the reason in the status line
    let blocker = dir.join("not-a-folder");
    std::fs::write(&blocker, b"").unwrap();
    d.click("openProject.save");
    answer(&mut d, vec![blocker.join("saved.fcproj").to_string_lossy().into_owned()]);
    assert!(d.has("openProject.save"));
    assert!(!d.app().ui.status.is_empty());
    assert_eq!(d.path(), None);
    assert!(d.app().session.is_dirty());
    // the file is picked a few frames later: saved, then the chosen project opens
    d.click("openProject.save");
    d.frames(3);
    assert_eq!(d.path(), None, "nothing opens before the answer");
    answer(&mut d, vec![saved.clone()]);
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.path(), Some(other.clone()));
    assert!(!d.app().session.is_dirty());
    d.exec("file.open", json!({"path": saved}));
    assert_eq!(d.label(clip), Label::Yellow, "the change was saved");
    // a new project changed while the Save dialog is open: the late answer saves and opens nothing
    let late = dir.join("late.fcproj").to_string_lossy().into_owned();
    d.app().session.path = None;
    d.exec("file.newBin", json!({"name": "Late"}));
    assert!(d.app().session.is_dirty());
    d.menu("file.open");
    answer(&mut d, vec![other]);
    d.click("openProject.save");
    d.exec("file.newProject", json!({"name": "New"}));
    d.exec("file.newBin", json!({"name": "Changed"}));
    answer(&mut d, vec![late.clone()]);
    assert!(!std::path::Path::new(&late).exists());
    assert_eq!(d.path(), None);
    assert!(d.app().session.is_dirty());
    assert!(d.app().ui.clip_dialog.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_save_prompt_closes_when_a_script_replaces_its_project() {
    let mut d = Driver::new();
    let (dir, _, other) = projects(&mut d, "replaced");
    // Open Project
    d.edit();
    d.open_project(&other);
    assert!(d.has("openProject.dontSave"));
    d.exec("file.newProject", json!({"name": "New"}));
    d.exec("file.newBin", json!({"name": "Changed"}));
    assert!(d.app().ui.clip_dialog.is_none());
    assert!(!d.has("openProject.dontSave"));
    assert_eq!(d.path(), None);
    assert!(d.app().session.is_dirty(), "the new project's change is kept");
    // Close Project
    d.menu("file.closeProject");
    assert!(d.has("closeProject.dontSave"));
    d.exec("file.newProject", json!({"name": "New"}));
    d.exec("file.newBin", json!({"name": "Changed"}));
    assert!(d.app().ui.clip_dialog.is_none());
    assert!(d.app().session.is_dirty());
    // Quit
    d.harness.input_mut().viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    d.frames(3);
    assert!(d.has("quit.dontSave"));
    d.exec("file.newProject", json!({"name": "New"}));
    d.exec("file.newBin", json!({"name": "Changed"}));
    assert!(d.app().ui.clip_dialog.is_none());
    assert!(!d.app().quit_confirmed);
    assert!(d.app().session.is_dirty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn home_screen_shows_new_open_recent_and_file_home_navigation() {
    let mut d = Driver::new();
    let (dir, current, other) = projects(&mut d, "homeNav");

    // File ▸ Home is listed at the top of the File menu
    let menus = d.ok("ui.menu.list", json!({}));
    let file_items: Vec<&Value> = menus.as_array().unwrap().iter().filter(|m| m["path"].as_array().and_then(|p| p.first()) == Some(&json!("File"))).collect();
    assert_eq!(file_items.first().and_then(|m| m["id"].as_str()), Some("mode.home"));
    assert_eq!(file_items.first().and_then(|m| m["label"].as_str()), Some("Home"));

    // Mid-session File ▸ Home keeps the active project and unsaved edits intact
    let clip = d.edit();
    d.menu("mode.home");
    assert_eq!(d.app().ui.mode, Mode::Import);
    assert_eq!(d.path(), Some(current.clone()));
    assert!(d.app().session.is_dirty());
    assert!(d.has("import.file.newProject"));
    assert!(d.has("import.file.open"));
    assert!(d.has("import.currentProject"));
    let i = d.recent(&other);
    assert!(d.has(&format!("import.recent.{i}")));

    // Clicking the current-project button returns to Edit without dropping changes
    d.click("import.currentProject");
    assert_eq!(d.app().ui.mode, Mode::Edit);
    assert_eq!(d.label(clip), Label::Yellow);
    assert!(d.app().session.is_dirty());

    // Save cleanly, then File ▸ Close Project returns to Home
    d.menu("file.save");
    assert!(!d.app().session.is_dirty());
    d.menu("file.closeProject");
    assert_eq!(d.app().ui.mode, Mode::Import);
    assert_eq!(d.path(), None);
    assert!(d.app().session.project.items.is_empty());

    // Clean Home: Open Project… cancelled stays on Home; choosing a project opens it in Edit
    d.app().hooks.pick_open_project = Some(Box::new(|| None));
    d.click("import.file.open");
    assert_eq!(d.app().ui.mode, Mode::Import);
    assert_eq!(d.path(), None);

    let other_pick = other.clone();
    d.app().hooks.pick_open_project = Some(Box::new(move || Some(other_pick.clone())));
    d.click("import.file.open");
    assert_eq!(d.app().ui.mode, Mode::Edit);
    assert_eq!(d.path(), Some(other));

    // Clean Home: New Project creates a clean project and enters Edit
    d.menu("mode.home");
    assert_eq!(d.app().ui.mode, Mode::Import);
    d.click("import.file.newProject");
    assert_eq!(d.app().ui.mode, Mode::Edit);
    assert_eq!(d.path(), None);
    assert!(d.app().session.project.items.is_empty());
    assert!(!d.app().session.is_dirty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_project_with_unsaved_changes_asks_first() {
    let mut d = Driver::new();
    let (dir, current, _) = projects(&mut d, "newProjectPrompt");
    let clip = d.edit();

    // File ▸ New ▸ Project… prompts when dirty; Cancel keeps edits
    assert_eq!(d.menu("file.newProject")["dialog"], "newProject");
    assert!(d.has("newProject.cancel") && d.has("newProject.dontSave") && d.has("newProject.save"));
    d.click("newProject.cancel");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.path(), Some(current.clone()));
    assert!(d.app().session.is_dirty());
    assert_eq!(d.label(clip), Label::Yellow);

    // Save writes changes to disk first, then creates a fresh clean project
    d.menu("file.newProject");
    d.click("newProject.save");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.path(), None);
    assert!(d.app().session.project.items.is_empty());
    assert!(!d.app().session.is_dirty());
    d.exec("file.open", json!({"path": current}));
    assert_eq!(d.label(clip), Label::Yellow, "edits were saved before New Project");

    // From Home on a never-saved dirty project: Save triggers Save As, then creates new project in Edit
    d.app().session.path = None;
    let first = d.app().session.active_sequence().unwrap().video_tracks[0].items[0].id;
    d.exec("timeline.select", json!({"clips": [first.0]}));
    d.menu("edit.label.rose");
    assert!(d.app().session.is_dirty());
    let saved_copy = dir.join("saved-before-new.fcproj").to_string_lossy().into_owned();
    let pick_path = saved_copy.clone();
    d.app().hooks.pick_save = Some(Box::new(move |_| Some(pick_path.clone())));
    d.menu("mode.home");
    assert_eq!(d.app().ui.mode, Mode::Import);
    d.click("import.file.newProject");
    assert!(d.has("newProject.save"));
    assert_eq!(d.app().ui.mode, Mode::Import, "Home stays while asking");
    d.click("newProject.save");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.app().ui.mode, Mode::Edit);
    assert_eq!(d.path(), None);
    assert!(d.app().session.project.items.is_empty());
    assert!(std::path::Path::new(&saved_copy).exists(), "never-saved project was saved first");
    d.exec("file.open", json!({"path": saved_copy}));
    assert_eq!(d.label(first), Label::Rose);

    // Don't Save discards edits and switches to a clean project
    d.exec("timeline.select", json!({"clips": [first.0]}));
    d.menu("edit.label.yellow");
    assert!(d.app().session.is_dirty());
    d.menu("file.newProject");
    d.click("newProject.dontSave");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.path(), None);
    assert!(d.app().session.project.items.is_empty());
    assert!(!d.app().session.is_dirty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_demo_project_with_unsaved_changes_asks_first() {
    let mut d = Driver::new();
    let (dir, current, _) = projects(&mut d, "demoPrompt");
    let clip = d.edit();

    // From Home: clicking the Demo project card asks before replacing unsaved work
    d.menu("mode.home");
    assert_eq!(d.app().ui.mode, Mode::Import);
    d.click("import.file.openDemoProject");
    assert!(d.has("openDemoProject.cancel") && d.has("openDemoProject.dontSave") && d.has("openDemoProject.save"));
    assert_eq!(d.app().ui.mode, Mode::Import, "Home stays while asking");
    assert_eq!(d.path(), Some(current.clone()));

    // Cancel keeps dirty project and stays on Home
    d.click("openDemoProject.cancel");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.app().ui.mode, Mode::Import);
    assert_eq!(d.path(), Some(current.clone()));
    assert!(d.app().session.is_dirty());

    // Save writes edits to disk, then loads Demo Project in Edit
    d.click("import.file.openDemoProject");
    d.click("openDemoProject.save");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.app().ui.mode, Mode::Edit);
    assert_eq!(d.path(), None);
    assert!(!d.app().session.is_dirty());
    assert!(!d.app().session.project.items.is_empty());

    // Confirm saved edits persisted on disk
    d.exec("file.open", json!({"path": current}));
    assert_eq!(d.label(clip), Label::Yellow);

    // Don't Save from File ▸ New ▸ Demo Project discards edits cleanly
    d.exec("timeline.select", json!({"clips": [clip.0]}));
    d.menu("edit.label.rose");
    assert!(d.app().session.is_dirty());
    assert_eq!(d.menu("file.openDemoProject")["dialog"], "openDemoProject");
    d.click("openDemoProject.dontSave");
    assert!(d.app().ui.clip_dialog.is_none());
    assert_eq!(d.path(), None);
    assert!(!d.app().session.is_dirty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn home_open_project_button_and_close_project_prompt_behave_consistently() {
    let mut d = Driver::new();
    let (dir, current, other) = projects(&mut d, "homeOpenAndClose");
    d.edit();

    // From Home, clicking Open Project… with dirty edits asks after picking the file
    d.menu("mode.home");
    let pick_other = other.clone();
    d.app().hooks.pick_open_project = Some(Box::new(move || Some(pick_other.clone())));
    d.click("import.file.open");
    assert!(d.has("openProject.dontSave"));
    assert_eq!(d.app().ui.mode, Mode::Import);
    assert_eq!(d.path(), Some(current.clone()));
    d.click("openProject.cancel");
    assert_eq!(d.app().ui.mode, Mode::Import);
    assert_eq!(d.path(), Some(current.clone()));
    assert!(d.app().session.is_dirty());

    let pick_other = other.clone();
    d.app().hooks.pick_open_project = Some(Box::new(move || Some(pick_other.clone())));
    d.click("import.file.open");
    d.click("openProject.dontSave");
    assert_eq!(d.app().ui.mode, Mode::Edit);
    assert_eq!(d.path(), Some(other));
    assert!(!d.app().session.is_dirty());

    // Dirty Close Project: Cancel stays in Edit, Don't Save closes and returns to Home
    d.edit();
    d.menu("file.closeProject");
    assert!(d.has("closeProject.dontSave"));
    d.click("closeProject.cancel");
    assert_eq!(d.app().ui.mode, Mode::Edit);
    assert!(d.app().session.is_dirty());

    d.menu("file.closeProject");
    d.click("closeProject.dontSave");
    assert_eq!(d.app().ui.mode, Mode::Import);
    assert_eq!(d.path(), None);
    assert!(!d.app().session.is_dirty());

    let _ = std::fs::remove_dir_all(&dir);
}
