//! Вторая колонка UI: название сервера, категории каналов, текстовые/голосовые каналы, блок профиля внизу.
//! Discord-like: анимация выбора канала (вертикальная полоска слева), сворачиваемые категории.
//! Нижний блок профиля (аватар, ник, mute, deafen, settings) — из bottom_panel.

use std::collections::HashMap;

use eframe::egui;

use crate::bottom_panel::{self, BottomPanelAction, BottomPanelParams, BottomPanelVoiceSnapshot};
use crate::net::VoiceParticipant;
use crate::theme::Theme;

/// Ширина панели каналов (как в Discord).
pub const CHANNEL_PANEL_WIDTH: f32 = 240.0;
/// Высота заголовка сервера.
const SERVER_HEADER_HEIGHT: f32 = 40.0;
/// Ширина вертикальной полоски у выбранного канала.
const CHANNEL_INDICATOR_WIDTH: f32 = 4.0;
const CHANNEL_INDICATOR_RADIUS: f32 = 2.0;

/// Действие пользователя в панели каналов (обрабатывается в ui.rs).
#[derive(Debug, Clone)]
pub enum ChannelPanelAction {
    SelectChannel(i64),
    JoinVoice { channel_id: i64, server_id: i64 },
    LeaveVoice,
    SetMicMuted(bool),
    SetOutputMuted(bool),
    CreateChannel,
    Invite,
    ChannelSettings(i64, String),
    OpenSettings,
    Logout,
    RetryChannels,
}

/// Снимок голосового состояния для отрисовки (без мутабельных заимствований).
#[derive(Clone, Default)]
pub struct ChannelPanelVoiceSnapshot {
    pub channel_id: Option<i64>,
    pub server_id: Option<i64>,
    pub mic_muted: bool,
    pub output_muted: bool,
    pub channel_voice: HashMap<i64, Vec<VoiceParticipant>>,
    pub speaking: HashMap<i64, bool>,
}

/// Состояние загрузки (для отображения спиннера/ошибки).
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelsLoadState {
    Idle,
    Loading,
    Loaded,
    Error(String),
}

/// Параметры для отрисовки панели каналов.
pub struct ChannelPanelParams<'a> {
    pub theme: &'a Theme,
    pub server_name: &'a str,
    pub server_id: i64,
    /// Текстовые каналы: (id, name).
    pub text_channels: &'a [(i64, String)],
    /// Голосовые каналы: (id, name).
    pub voice_channels: &'a [(i64, String)],
    pub selected_channel_id: Option<i64>,
    pub voice: ChannelPanelVoiceSnapshot,
    pub user_display: &'a str,
    pub user_id: Option<i64>,
    pub on_action: &'a mut dyn FnMut(ChannelPanelAction),
    /// Аватар текущего пользователя (опционально).
    pub avatar_texture: Option<&'a egui::TextureHandle>,
    pub channels_load: ChannelsLoadState,
}

/// Отрисовка второй колонки: заголовок сервера, список каналов, профиль внизу.
pub fn show(ctx: &egui::Context, ui: &mut egui::Ui, params: ChannelPanelParams<'_>) {
    let ChannelPanelParams {
        theme,
        server_name,
        server_id,
        text_channels,
        voice_channels,
        selected_channel_id,
        voice,
        user_display,
        user_id,
        on_action,
        avatar_texture,
        channels_load,
    } = params;

    // ─── Блок профиля внизу (bottom_panel) ──────────────────────────────────
    let profile_height = bottom_panel::BOTTOM_PANEL_HEIGHT;
    let scroll_h = (ui.available_height() - profile_height).max(60.0);

    let voice_snap = BottomPanelVoiceSnapshot {
        in_voice_channel: voice.channel_id.is_some(),
        mic_muted: voice.mic_muted,
        output_muted: voice.output_muted,
    };

    egui::TopBottomPanel::bottom("channel_panel_user")
        .show_separator_line(true)
        .show_inside(ui, |ui| {
            let mut map_action = |a: BottomPanelAction| {
                match a {
                    BottomPanelAction::LeaveVoice => (*on_action)(ChannelPanelAction::LeaveVoice),
                    BottomPanelAction::SetMicMuted(b) => (*on_action)(ChannelPanelAction::SetMicMuted(b)),
                    BottomPanelAction::SetOutputMuted(b) => (*on_action)(ChannelPanelAction::SetOutputMuted(b)),
                    BottomPanelAction::OpenSettings => (*on_action)(ChannelPanelAction::OpenSettings),
                    BottomPanelAction::Logout => (*on_action)(ChannelPanelAction::Logout),
                }
            };
            bottom_panel::show(ctx, ui, BottomPanelParams {
                theme,
                user_display,
                user_id,
                voice: voice_snap,
                avatar_texture,
                on_action: &mut map_action,
            });
        });

    // ─── Заголовок сервера + кнопки ─────────────────────────────────────────
    egui::TopBottomPanel::top("channel_panel_header")
        .exact_height(SERVER_HEADER_HEIGHT)
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new(server_name).color(theme.text_primary));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("+").on_hover_text("Создать канал").clicked() {
                        (*on_action)(ChannelPanelAction::CreateChannel);
                    }
                    if ui.small_button("👥").on_hover_text("Пригласить").clicked() {
                        (*on_action)(ChannelPanelAction::Invite);
                    }
                });
            });
        });

    // ─── Список каналов (скролл) ────────────────────────────────────────────
    egui::ScrollArea::vertical()
        .id_source("channel_panel_scroll")
        .max_height(scroll_h)
        .show(ui, |ui| {
            match channels_load {
                ChannelsLoadState::Loading => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0);
                        ui.spinner();
                        ui.label(egui::RichText::new("Загрузка каналов...").color(theme.text_muted));
                    });
                    return;
                }
                ChannelsLoadState::Error(ref msg) => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0);
                        ui.label(egui::RichText::new(msg).color(theme.error));
                        ui.add_space(8.0);
                        if ui.button("Повторить").clicked() {
                            (*on_action)(ChannelPanelAction::RetryChannels);
                        }
                    });
                    return;
                }
                _ => {}
            }
            ui.label(
                egui::RichText::new("ТЕКСТОВЫЕ КАНАЛЫ")
                    .small()
                    .color(theme.text_muted),
            );
            for (id, name) in text_channels.iter() {
                let sel = selected_channel_id == Some(*id);
                let resp = channel_row(
                    ctx,
                    ui,
                    theme,
                    *id,
                    name,
                    &format!("# {}", name),
                    sel,
                    false,
                    false,
                    format!("ch_text_{}", id),
                );
                if resp.clicked() {
                    (*on_action)(ChannelPanelAction::SelectChannel(*id));
                }
                resp.context_menu(|ui| {
                    if ui.button("Настройки канала").clicked() {
                        (*on_action)(ChannelPanelAction::ChannelSettings(*id, name.clone()));
                        ui.close_menu();
                    }
                });
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("ГОЛОСОВЫЕ КАНАЛЫ")
                    .small()
                    .color(theme.text_muted),
            );

            for (id, name) in voice_channels.iter() {
                let sel = selected_channel_id == Some(*id);
                let in_this = voice.channel_id == Some(*id);
                let label_text = if in_this {
                    format!("🔊 {}", name)
                } else {
                    format!("🔈 {}", name)
                };
                let resp = channel_row(
                    ctx,
                    ui,
                    theme,
                    *id,
                    name,
                    &label_text,
                    sel || in_this,
                    true,
                    in_this,
                    format!("ch_voice_{}", id),
                );
                if resp.clicked() {
                    (*on_action)(ChannelPanelAction::SelectChannel(*id));
                    if !in_this {
                        (*on_action)(ChannelPanelAction::JoinVoice {
                            channel_id: *id,
                            server_id,
                        });
                    }
                }
                resp.context_menu(|ui| {
                    if ui.button("Настройки канала").clicked() {
                        (*on_action)(ChannelPanelAction::ChannelSettings(*id, name.clone()));
                        ui.close_menu();
                    }
                });

                // Участники голосового канала (упрощённо: список имён)
                let participants = voice.channel_voice.get(id).cloned().unwrap_or_default();
                    for p in participants.iter() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        let is_speaking = *voice.speaking.get(&p.user_id).unwrap_or(&false);
                        crate::components::avatar::avatar(ui, theme, &p.username, 10.0, is_speaking, None);
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&p.username)
                                .small()
                                .color(theme.text_secondary),
                        );
                        if p.mic_muted {
                            ui.label(egui::RichText::new("🔇").small());
                        }
                    });
                }
            }
        });
}

/// Строка канала с анимацией выбора (вертикальная полоска слева).
fn channel_row(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    id: i64,
    _name: &str,
    label: &str,
    selected: bool,
    _is_voice: bool,
    _in_this_voice: bool,
    id_source: impl std::hash::Hash,
) -> egui::Response {
    let id = ui.make_persistent_id(id_source);
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(width, 24.0),
        egui::Sense::click(),
    );
    let hovered = resp.hovered();
    let active_id = id.with("active");
    let active_t = ctx.animate_bool(active_id, selected);
    let hover_id = id.with("hover");
    let hover_t = ctx.animate_bool(hover_id, hovered);

    let bg = if selected {
        Theme::lerp_color(theme.bg_secondary, theme.bg_hover, active_t * 0.5 + 0.5)
    } else {
        Theme::lerp_color(theme.bg_secondary, theme.bg_hover, hover_t)
    };
    ui.painter().rect_filled(rect, 0.0, bg);

    if active_t > 0.0 {
        let bar_left = rect.min.x + 2.0;
        let bar_rect = egui::Rect::from_min_size(
            egui::pos2(bar_left, rect.center().y - 8.0),
            egui::vec2(CHANNEL_INDICATOR_WIDTH, 16.0),
        );
        let bar_fill = Theme::lerp_color(theme.bg_secondary, theme.accent, active_t);
        ui.painter().rect_filled(bar_rect, CHANNEL_INDICATOR_RADIUS, bar_fill);
    }

    let text_color = if selected {
        theme.text_primary
    } else {
        theme.text_secondary
    };
    let galley = ui.painter().layout(
        label.to_string(),
        egui::FontId::proportional(14.0),
        text_color,
        rect.width() - 16.0,
    );
    ui.painter().galley(
        rect.min + egui::vec2(12.0, (rect.height() - galley.size().y) * 0.5),
        galley,
        text_color,
    );
    resp
}
