//! Нижняя панель (Voice/Status bar): аватар, ник, статус, Mute, Deafen, Settings.
//! Discord-like: отображается внизу колонки каналов, привязка Mute/Deafen к voice.

use eframe::egui;

use crate::theme::Theme;

/// Радиус аватарки в нижней панели.
const AVATAR_RADIUS: f32 = 16.0;
/// Высота панели (для расчёта скролла в родителе).
pub const BOTTOM_PANEL_HEIGHT: f32 = 52.0;

/// Действие пользователя в нижней панели (обрабатывается в ui.rs через channel_panel).
#[derive(Debug, Clone)]
pub enum BottomPanelAction {
    LeaveVoice,
    SetMicMuted(bool),
    SetOutputMuted(bool),
    OpenSettings,
    Logout,
}

/// Снимок голосового состояния для нижней панели.
#[derive(Clone, Default)]
pub struct BottomPanelVoiceSnapshot {
    pub in_voice_channel: bool,
    pub mic_muted: bool,
    pub output_muted: bool,
}

/// Параметры для отрисовки нижней панели.
pub struct BottomPanelParams<'a> {
    pub theme: &'a Theme,
    pub user_display: &'a str,
    pub user_id: Option<i64>,
    pub voice: BottomPanelVoiceSnapshot,
    pub avatar_texture: Option<&'a egui::TextureHandle>,
    pub on_action: &'a mut dyn FnMut(BottomPanelAction),
}

/// Отрисовка нижней панели: аватар, ник, Mute, Deafen, Settings.
/// Вызывается из channel_panel внутри его TopBottomPanel::bottom.
pub fn show(ctx: &egui::Context, ui: &mut egui::Ui, params: BottomPanelParams<'_>) {
    let _ = ctx;
    let BottomPanelParams {
        theme,
        user_display,
        user_id,
        voice,
        avatar_texture,
        on_action,
    } = params;

    ui.add_space(6.0);
    ui.horizontal(|ui| {
                let _ = crate::components::avatar::avatar(ui, theme, user_display, AVATAR_RADIUS, false, avatar_texture);
                ui.add_space(6.0);

                // Ник (клик — копировать ID)
                let name_btn = ui.button(
                    egui::RichText::new(user_display).strong().color(theme.text_primary),
                )
                .on_hover_text(
                    user_id
                        .map(|id| format!("ID: {} — нажмите чтобы скопировать", id))
                        .unwrap_or_default(),
                );
                if name_btn.clicked() {
                    if let Some(uid) = user_id {
                        ui.ctx().output_mut(|o| o.copied_text = uid.to_string());
                    }
                }

                ui.add_space(4.0);

                // Кнопки: Leave (если в голосе), Mute, Deafen, Settings, Logout
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("🚪").on_hover_text("Выйти из аккаунта").clicked() {
                        (*on_action)(BottomPanelAction::Logout);
                    }
                    if ui.small_button("⚙").on_hover_text("Настройки").clicked() {
                        (*on_action)(BottomPanelAction::OpenSettings);
                    }

                    if voice.in_voice_channel {
                        if ui.small_button("📵").on_hover_text("Отключиться от голоса").clicked() {
                            (*on_action)(BottomPanelAction::LeaveVoice);
                        }
                    }

                    // Mute / Deafen — всегда видны, привязка к voice (состояние сохраняется при входе)
                    let out_label = if voice.output_muted { "🔇" } else { "🔊" };
                    let out_tip = if voice.output_muted {
                        "Включить звук"
                    } else {
                        "Выключить звук"
                    };
                    if ui.small_button(out_label).on_hover_text(out_tip).clicked() {
                        (*on_action)(BottomPanelAction::SetOutputMuted(!voice.output_muted));
                    }

                    let mic_label = if voice.mic_muted { "🎤🔇" } else { "🎤" };
                    let mic_tip = if voice.mic_muted {
                        "Включить микрофон"
                    } else {
                        "Выключить микрофон"
                    };
                    if ui.small_button(mic_label).on_hover_text(mic_tip).clicked() {
                        (*on_action)(BottomPanelAction::SetMicMuted(!voice.mic_muted));
                    }
                });
            });
    ui.add_space(6.0);
}
