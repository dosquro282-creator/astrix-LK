//! Стилизованные кнопки с анимацией: круглые иконки, Theme.

use eframe::egui;

use crate::theme::Theme;

/// Радиус круглой кнопки по умолчанию.
const DEFAULT_ICON_RADIUS: f32 = 24.0;
/// Ширина вертикальной полоски при активном состоянии.
const ACTIVE_INDICATOR_WIDTH: f32 = 4.0;
const ACTIVE_INDICATOR_RADIUS: f32 = 2.0;

/// Круглая кнопка с иконкой/текстом: hover-анимация, вертикальная полоска слева при active.
/// Используется для серверов, Home, Add, Explore и т.д.
pub fn icon_circle(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    tooltip: &str,
    active: bool,
    id_source: impl std::hash::Hash,
) -> egui::Response {
    icon_circle_sized(ctx, ui, theme, DEFAULT_ICON_RADIUS, label, tooltip, active, id_source)
}

/// То же, что icon_circle, с заданным радиусом.
pub fn icon_circle_sized(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    radius: f32,
    label: &str,
    tooltip: &str,
    active: bool,
    id_source: impl std::hash::Hash,
) -> egui::Response {
    let size = egui::vec2(radius * 2.0, radius * 2.0);
    let id = ui.make_persistent_id(id_source);

    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let hovered = resp.hovered();
    let resp = resp.on_hover_text(tooltip);

    let hover_id = id.with("hover");
    let hover_t = ctx.animate_bool(hover_id, hovered);
    let active_id = id.with("active");
    let active_t = ctx.animate_bool(active_id, active);

    if active_t > 0.0 {
        let bar_left = rect.min.x - ACTIVE_INDICATOR_WIDTH - 8.0;
        let bar_top = rect.center().y - (rect.height() * 0.5 + 4.0);
        let bar_rect = egui::Rect::from_min_size(
            egui::pos2(bar_left, bar_top),
            egui::vec2(ACTIVE_INDICATOR_WIDTH, rect.height() + 8.0),
        );
        let bar_fill = Theme::lerp_color(theme.bg_secondary, theme.accent, active_t);
        ui.painter().rect_filled(bar_rect, ACTIVE_INDICATOR_RADIUS, bar_fill);
    }

    let bg_normal = theme.bg_secondary;
    let bg_hover = theme.bg_hover;
    let bg_active_color = theme.accent;
    let fill = if active {
        Theme::lerp_color(bg_hover, bg_active_color, active_t * 0.3 + 0.7)
    } else {
        Theme::lerp_color(bg_normal, bg_hover, hover_t)
    };
    ui.painter().circle_filled(rect.center(), radius, fill);
    ui.painter().circle_stroke(
        rect.center(),
        radius,
        egui::Stroke::new(1.0, theme.border),
    );

    let font_size = (radius * 0.85).max(10.0);
    let galley = ui.painter().layout(
        label.to_string(),
        egui::FontId::proportional(font_size),
        theme.text_primary,
        f32::INFINITY,
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, theme.text_primary);

    resp
}
