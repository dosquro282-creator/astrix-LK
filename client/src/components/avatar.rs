//! Компонент аватара: буква по имени или текстура, опционально speaking-indicator.

use eframe::egui;

use crate::theme::Theme;

/// Рисует аватар в текущем layout (выделяет место, рисует круг/текстуру).
/// Возвращает Response для hover/click при необходимости.
pub fn avatar(
    ui: &mut egui::Ui,
    theme: &Theme,
    display_name: &str,
    radius: f32,
    speaking: bool,
    texture: Option<&egui::TextureHandle>,
) -> egui::Response {
    let ring_margin = if speaking { 3.0 } else { 0.0 };
    let size = egui::vec2((radius + ring_margin) * 2.0, (radius + ring_margin) * 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    paint_at(
        ui,
        theme,
        rect.center(),
        display_name,
        radius,
        speaking,
        texture,
    );
    resp
}

/// Рисует аватар в заданной позиции (без выделения места).
/// Для случаев, когда layout управляется вручную (например, member_row).
pub fn avatar_at(
    ui: &mut egui::Ui,
    theme: &Theme,
    center: egui::Pos2,
    display_name: &str,
    radius: f32,
    speaking: bool,
    texture: Option<&egui::TextureHandle>,
) {
    paint_at(ui, theme, center, display_name, radius, speaking, texture);
}

fn paint_at(
    ui: &mut egui::Ui,
    theme: &Theme,
    center: egui::Pos2,
    display_name: &str,
    radius: f32,
    speaking: bool,
    texture: Option<&egui::TextureHandle>,
) {
    let circle_rect = egui::Rect::from_center_size(center, egui::vec2(radius * 2.0, radius * 2.0));

    if let Some(tex) = texture {
        let img = egui::Image::new(tex).fit_to_exact_size(circle_rect.size());
        img.paint_at(ui, circle_rect);
    } else {
        let letter = display_name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());
        let avatar_bg = Theme::lerp_color(theme.bg_secondary, theme.accent, 0.4);
        ui.painter().circle_filled(center, radius, avatar_bg);
        let font_size = (radius * 0.85).max(9.0);
        let galley = ui.painter().layout(
            letter,
            egui::FontId::proportional(font_size),
            theme.text_primary,
            f32::INFINITY,
        );
        let pos = center - galley.size() / 2.0;
        ui.painter().galley(pos, galley, theme.text_primary);
    }

    if speaking {
        ui.painter()
            .circle_stroke(center, radius + 1.5, egui::Stroke::new(2.5, theme.success));
    }
}
