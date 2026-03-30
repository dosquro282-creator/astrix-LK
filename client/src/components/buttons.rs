//! Shared button painters for the Discord-like navigation chrome.

use eframe::egui;

use crate::theme::Theme;

const DEFAULT_ICON_RADIUS: f32 = 24.0;
const ACTIVE_INDICATOR_WIDTH: f32 = 8.0;
const ACTIVE_INDICATOR_RADIUS: f32 = 4.0;

pub fn icon_circle(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    tooltip: &str,
    active: bool,
    id_source: impl std::hash::Hash,
) -> egui::Response {
    icon_circle_sized_with_label_size(
        ctx,
        ui,
        theme,
        DEFAULT_ICON_RADIUS,
        None,
        label,
        tooltip,
        active,
        id_source,
    )
}

pub fn icon_circle_with_label_size(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    tooltip: &str,
    active: bool,
    label_size: f32,
    id_source: impl std::hash::Hash,
) -> egui::Response {
    icon_circle_sized_with_label_size(
        ctx,
        ui,
        theme,
        DEFAULT_ICON_RADIUS,
        Some(label_size),
        label,
        tooltip,
        active,
        id_source,
    )
}

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
    icon_circle_sized_with_label_size(
        ctx, ui, theme, radius, None, label, tooltip, active, id_source,
    )
}

fn icon_circle_sized_with_label_size(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    radius: f32,
    label_size: Option<f32>,
    label: &str,
    tooltip: &str,
    active: bool,
    id_source: impl std::hash::Hash,
) -> egui::Response {
    let size = egui::vec2(radius * 2.0, radius * 2.0);
    let id = ui.make_persistent_id(id_source);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let hovered = response.hovered();
    let response = response.on_hover_text(tooltip);

    let hover_t = ctx.animate_bool(id.with("hover"), hovered);
    let active_t = ctx.animate_bool(id.with("active"), active);

    if active_t > 0.0 {
        let indicator_height = egui::lerp(8.0..=rect.height(), active_t);
        let indicator_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() - 10.0, rect.center().y),
            egui::vec2(ACTIVE_INDICATOR_WIDTH, indicator_height),
        );
        ui.painter()
            .rect_filled(indicator_rect, ACTIVE_INDICATOR_RADIUS, theme.text_primary);
    }

    let fill = if active {
        Theme::lerp_color(theme.bg_hover, theme.accent, 0.75 + active_t * 0.25)
    } else {
        Theme::lerp_color(theme.bg_secondary, theme.bg_hover, hover_t)
    };
    let rounding = if active || hovered {
        egui::Rounding::same(radius * 0.35)
    } else {
        egui::Rounding::same(radius)
    };

    ui.painter().rect_filled(rect, rounding, fill);

    let text_color = if active {
        theme.text_primary
    } else {
        Theme::lerp_color(theme.text_secondary, theme.text_primary, hover_t)
    };
    let font_size = label_size.unwrap_or_else(|| (radius * 0.72).clamp(14.0, 22.0));
    let galley = ui.painter().layout(
        label.to_string(),
        egui::FontId::proportional(font_size),
        text_color,
        rect.width(),
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, text_color);

    response
}
