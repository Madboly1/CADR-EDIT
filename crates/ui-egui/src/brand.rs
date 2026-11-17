//! CADR Studio wordmark rendered natively so the app does not depend on upstream branding.

/// Kept for compatibility with existing callers that request a texture wordmark.
/// The CADR Studio wordmark is painted as text by paint_wordmark.
pub fn wordmark(_ctx: &egui::Context, _dark: bool) -> Option<egui::TextureHandle> {
    None
}

/// Draw the CADR Studio wordmark and return its bounds.
pub fn paint_wordmark(ui: &egui::Ui, left_center: egui::Pos2, height: f32, dark: bool) -> Option<egui::Rect> {
    let width = height * 4.1;
    let rect = egui::Rect::from_min_size(
        egui::pos2(left_center.x, left_center.y - height / 2.0),
        egui::vec2(width, height),
    );
    let color = if dark { egui::Color32::WHITE } else { egui::Color32::from_rgb(20, 20, 22) };
    ui.painter().text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        "CADR Studio",
        egui::FontId::proportional(height * 0.72),
        color,
    );
    Some(rect)
}
