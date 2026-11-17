//! Generic browser-error dialog support. Legacy community and project URLs are not part of CADR Studio.

/// A link the web browser couldn't open (`ui.failedLink`): the address and the reason.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FailedLink {
    pub url: String,
    pub error: String,
}

/// Hover descriptions for the remaining internal Help actions.
pub fn menu_hint(command: &str) -> Option<String> {
    Some(match command {
        "help.revealLogFiles" => tl!("Saves this session's log and shows it in your file manager."),
        "help.systemCompatibilityReport" => tl!("Shows a report on this computer."),
        "app.about" => tl!("Shows CADR Studio version and licence information."),
        "app.checkForUpdates" => tl!("Looks for a newer CADR Studio release and offers to update."),
        _ => return None,
    }.to_string())
}

/// The URL a `help.*` link command opens.
pub fn url_for(command: &str) -> Option<&'static str> {
    ALL.iter().find(|(id, _, _)| *id == command).map(|(_, _, u)| *u)
}

/// What a Help menu item does, shown when the pointer rests on it: just the address a link opens
/// in the web browser, or what an item that stays inside FilmCraft does.
pub fn menu_hint(command: &str) -> Option<String> {
    let url = if command == "help.filmcraftHelp" { Some(crate::panels::keyboard::HELP_URL) } else { url_for(command) };
    if let Some(url) = url {
        return Some(url.to_string());
    }
    Some(
        match command {
            "help.revealLogFiles" => tl!("Saves this session's log and shows it in your file manager."),
            "help.systemCompatibilityReport" => tl!("Shows a report on this computer."),
            "app.about" => tl!("Shows the version, community links and credits in a FilmCraft window."),
            "app.checkForUpdates" => tl!("Looks for a newer FilmCraft release and offers to update."),
            _ => return None,
        }
        .to_string(),
    )
}

/// The dialog for a link the browser couldn't open: the reason, and the address in a read-only
/// field you can select from, with Copy. It stays until Close or Escape, so the address is still
/// there after switching to another window. Automation ids: `failedLink.url`, `failedLink.copy`,
/// `failedLink.close`.
pub fn show_failed(app: &mut crate::FilmcraftApp, ctx: &egui::Context) {
    let Some(link) = app.ui.failed_link.clone() else { return };
    let mut close = false;
    let mut elems: Vec<(&str, egui::Rect, String)> = Vec::new();
    egui::Window::new(tl!("Couldn't open your web browser"))
        .id(egui::Id::unique("failed-link"))
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ui.set_max_width(460.0);
            if !link.error.is_empty() {
                ui.label(egui::RichText::new(&link.error).small());
            }
            ui.label(tl!("Open this address in your browser yourself:"));
            ui.horizontal(|ui| {
                let mut text = link.url.as_str();
                let r = crate::widgets::accessible_name(ui.add(egui::TextEdit::singleline(&mut text).desired_width(340.0)), "Link address");
                elems.push(("failedLink.url", r.rect, link.url.clone()));
                let c = ui.button(tl!("Copy"));
                elems.push(("failedLink.copy", c.rect, "Copy".into()));
                if c.clicked() {
                    ui.ctx().copy_text(link.url.clone());
                }
            });
            ui.add_space(6.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = ui.button(tl!("Close"));
                elems.push(("failedLink.close", r.rect, "Close".into()));
                close |= r.clicked();
            });
        });
    for (id, r, label) in elems {
        app.auto.add(id, r, &label);
    }
    if close || ctx.input(|i| i.key_pressed(egui::Key::Escape) && !i.pointer.any_down()) {
        app.ui.failed_link = None;
    }
}

