//! Правая колонка UI: список участников по ролям/онлайн, Voice speaking indicator.
//! Discord-like: группировка онлайн/оффлайн, подсветка говорящих, использование Theme.

use std::collections::HashMap;

use eframe::egui;

use crate::theme::Theme;

/// Ширина панели участников (как в Discord).
pub const MEMBER_PANEL_WIDTH: f32 = 220.0;
/// Радиус аватарки участника.
const AVATAR_RADIUS: f32 = 14.0;
/// Высота строки участника.
const MEMBER_ROW_HEIGHT: f32 = 32.0;

/// Снимок участника для отрисовки (без мутабельных заимствований).
#[derive(Clone)]
pub struct MemberSnapshot {
    pub user_id: i64,
    pub display_name: String,
    pub username: String,
    pub is_owner: bool,
    pub online: bool,
}

/// Параметры для отрисовки панели участников.
pub struct MemberPanelParams<'a> {
    pub theme: &'a Theme,
    pub members: &'a [MemberSnapshot],
    pub online_count: usize,
    /// Кто сейчас говорит (user_id -> true). Из voice.speaking.
    pub speaking: &'a HashMap<i64, bool>,
    /// Аватарки по user_id (опционально).
    pub avatar_textures: &'a HashMap<i64, egui::TextureHandle>,
}

/// Отрисовка правой колонки: заголовок, список участников с группировкой онлайн/оффлайн,
/// speaking indicator (подсветка говорящих).
pub fn show(ctx: &egui::Context, ui: &mut egui::Ui, params: MemberPanelParams<'_>) {
    let MemberPanelParams {
        theme,
        members,
        online_count,
        speaking,
        avatar_textures,
    } = params;

    // ─── Заголовок ─────────────────────────────────────────────────────────────
    egui::TopBottomPanel::top("members_header")
        .exact_height(40.0)
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("Участники").color(theme.text_primary));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("В сети — {}", online_count))
                            .size(13.0)
                            .color(theme.success),
                    );
                });
            });
        });

    // ─── Список участников (скролл) ────────────────────────────────────────────
    egui::ScrollArea::vertical()
        .id_source("member_panel_scroll")
        .show(ui, |ui| {
            if members.is_empty() {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("Нет участников.")
                        .color(theme.text_muted),
                );
                return;
            }

            let mut prev_online = true;
            for m in members.iter() {
                // Разделитель при переходе от онлайн к оффлайн
                if prev_online && !m.online {
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);
                }
                prev_online = m.online;

                let is_speaking = *speaking.get(&m.user_id).unwrap_or(&false);
                let _ = member_row(
                    ctx,
                    ui,
                    theme,
                    m,
                    is_speaking,
                    avatar_textures.get(&m.user_id),
                );
                ui.add_space(2.0);
            }
        });
}

/// Строка участника с анимацией hover и speaking indicator.
fn member_row(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    m: &MemberSnapshot,
    is_speaking: bool,
    avatar_texture: Option<&egui::TextureHandle>,
) -> egui::Response {
    let id = ui.make_persistent_id(("member", m.user_id));
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(width, MEMBER_ROW_HEIGHT),
        egui::Sense::hover(),
    );

    let hovered = resp.hovered();
    let hover_id = id.with("hover");
    let hover_t = ctx.animate_bool(hover_id, hovered);

    let bg = if hovered {
        Theme::lerp_color(theme.bg_secondary, theme.bg_hover, hover_t)
    } else {
        theme.bg_secondary
    };
    ui.painter().rect_filled(rect, 0.0, bg);

    // Аватар + speaking ring
    let avatar_x = rect.min.x + AVATAR_RADIUS + 4.0;
    let avatar_center = egui::pos2(avatar_x, rect.center().y);
    crate::components::avatar::avatar_at(
        ui,
        theme,
        avatar_center,
        &m.display_name,
        AVATAR_RADIUS,
        is_speaking,
        avatar_texture,
    );

    // Имя и роль
    let text_x = avatar_x + AVATAR_RADIUS + 8.0;
    let name_color = if m.online {
        theme.text_primary
    } else {
        theme.text_muted
    };
    let galley = ui.painter().layout(
        m.display_name.clone(),
        egui::FontId::proportional(14.0),
        name_color,
        rect.width() - (text_x - rect.min.x) - 8.0,
    );
    let galley_size = galley.size();
    ui.painter().galley(
        egui::pos2(text_x, rect.center().y - galley_size.y * 0.5),
        galley.clone(),
        name_color,
    );

    if m.is_owner {
        let name_w = galley_size.x + 4.0;
        let owner_pos = egui::pos2(text_x + name_w, rect.center().y - 6.0);
        ui.painter().text(
            owner_pos,
            egui::Align2::LEFT_CENTER,
            "[автор]",
            egui::FontId::proportional(11.0),
            theme.text_muted,
        );
    }

    resp.on_hover_text(format!("Логин: {}", m.username))
}
