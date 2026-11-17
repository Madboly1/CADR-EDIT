//! Headless UI regression test ensuring legacy community links are absent.

use std::sync::mpsc::{Sender, channel};
use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver { harness: Harness<'static, FilmcraftApp>, tx: Sender<ControlRequest> }
impl Driver {
    fn demo() -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Self { harness, tx }; d.frames(4); d
    }
    fn frames(&mut self, n: usize) { for _ in 0..n { let ctx=self.harness.ctx.clone(); let mut raw=std::mem::take(self.harness.input_mut()); eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw); *self.harness.input_mut()=raw; self.harness.step(); } }
    fn ok(&mut self, method:&str, params:Value)->Value { let(req,reply)=ControlRequest::new(method,params.clone()); self.tx.send(req).unwrap(); for _ in 0..600 { self.frames(1); if let Ok(v)=reply.try_recv(){assert_eq!(v["ok"],json!(true),"{method} {params}: {v}"); return v["result"].clone();}} panic!("no reply to {method} {params}"); }
    fn ids(&mut self,prefix:&str)->Vec<String>{self.ok("ui.elements",json!({"prefix":prefix})).as_array().unwrap().iter().filter_map(|e|e["id"].as_str().map(str::to_string)).collect()}
}

#[test]
fn legacy_community_links_are_absent_from_cadr_ui() {
    let mut d=Driver::demo();
    let menu=d.ok("ui.menu.list",json!({})).to_string();
    for old in ["help.discord","help.website","help.appPage","help.github","help.reportIssue","ArtCraft","getartcraft.com","discord.gg/artcraft"] { assert!(!menu.contains(old),"legacy item remains: {old}"); }
    assert!(d.ids("header.discord").is_empty());
    d.ok("engine.execute",json!({"command":"app.about","params":{}})); d.frames(3);
    let about=d.ids("about."); assert!(about.is_empty(),"legacy About links remain: {about:?}");
    d.ok("ui.key",json!({"key":"Escape"})); d.frames(2);
    d.ok("engine.execute",json!({"command":"mode.import","params":{}})); d.frames(3);
    assert!(d.ids("import.link.").is_empty());
}
