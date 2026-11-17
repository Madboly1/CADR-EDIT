//! CADR Studio brand mark and wordmark, painted natively to keep the UI crisp at every scale.

/// Kept for compatibility with existing callers that request a texture wordmark.
/// The mark and wordmark are painted natively by `paint_wordmark`.
pub fn wordmark(_ctx: &egui::Context, _dark: bool) -> Option<egui::TextureHandle> {
    None
}

/// Draw the CADR Studio mark and wordmark and return their combined bounds.
pub fn paint_wordmark(ui: &egui::Ui, left_center: egui::Pos2, height: f32, dark: bool) -> Option<egui::Rect> {
    if height <= 0.0 {
        return None;
    }

    let mark_size = height;
    let gap = height * 0.22;
    let width = height * 4.1;
    let rect = egui::Rect::from_min_size(egui::pos2(left_center.x, left_center.y - height / 2.0), egui::vec2(width, height));

    // Geometric open-C frame: light upper stroke, cool-grey lower stroke,
    // and the orange play triangle from the approved CADR Studio identity.
    let x = rect.left() + mark_size * 0.08;
    let y = rect.top() + mark_size * 0.08;
    let right = x + mark_size * 0.78;
    let top = y + mark_size * 0.12;
    let bottom = y + mark_size * 0.88;
    let left = x + mark_size * 0.12;
    let stroke = height * 0.14;
    let upper = if dark { egui::Color32::from_rgb(0xF4, 0xF4, 0xF5) } else { egui::Color32::from_rgb(0x24, 0x24, 0x28) };
    let lower = if dark { egui::Color32::from_rgb(0xD9, 0xDC, 0xE3) } else { egui::Color32::from_rgb(0x68, 0x6B, 0x73) };
    let orange = egui::Color32::from_rgb(0xFF, 0x64, 0x2F);

    let painter = ui.painter();
    painter.line_segment([egui::pos2(right, top), egui::pos2(left, top)], egui::Stroke::new(stroke, upper));
    painter.line_segment([egui::pos2(left, top), egui::pos2(left, bottom)], egui::Stroke::new(stroke, upper));
    painter.line_segment([egui::pos2(left, bottom), egui::pos2(right, bottom)], egui::Stroke::new(stroke, lower));

    let tri_left = x + mark_size * 0.48;
    let tri_center_y = rect.center().y;
    let triangle = vec![
        egui::pos2(tri_left, tri_center_y - mark_size * 0.23),
        egui::pos2(tri_left, tri_center_y + mark_size * 0.23),
        egui::pos2(tri_left + mark_size * 0.34, tri_center_y),
    ];
    painter.add(egui::Shape::convex_polygon(triangle, orange, egui::Stroke::NONE));

    let text_color = if dark { egui::Color32::from_rgb(0xF4, 0xF4, 0xF5) } else { egui::Color32::from_rgb(0x14, 0x14, 0x16) };
    painter.text(
        egui::pos2(rect.left() + mark_size + gap, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "CADR Studio",
        egui::FontId::proportional(height * 0.62),
        text_color,
    );

    Some(rect)
}
