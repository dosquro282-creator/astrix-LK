//! Unified bottom user strip for the left side of the app.

use eframe::egui;

use crate::theme::Theme;

const AVATAR_RADIUS: f32 = 16.0;
const CONTROL_SIZE: f32 = 28.0;

pub const BOTTOM_PANEL_HEIGHT: f32 = 58.0;

#[derive(Debug, Clone)]
pub enum BottomPanelAction {
    SetMicMuted(bool),
    SetOutputMuted(bool),
    OpenSettings,
}

#[derive(Clone, Default)]
pub struct BottomPanelVoiceSnapshot {
    pub in_voice_channel: bool,
    pub mic_muted: bool,
    pub output_muted: bool,
}

pub struct BottomPanelParams<'a> {
    pub theme: &'a Theme,
    pub user_display: &'a str,
    pub voice: BottomPanelVoiceSnapshot,
    pub avatar_texture: Option<&'a egui::TextureHandle>,
    pub on_action: &'a mut dyn FnMut(BottomPanelAction),
}

pub fn show(_ctx: &egui::Context, ui: &mut egui::Ui, params: BottomPanelParams<'_>) {
    let BottomPanelParams {
        theme,
        user_display,
        voice,
        avatar_texture,
        on_action,
    } = params;

    let rect = ui.max_rect();
    ui.painter()
        .rect_filled(rect, egui::Rounding::ZERO, theme.bg_quaternary);

    let inner = rect.shrink2(egui::vec2(12.0, 8.0));
    ui.allocate_ui_at_rect(inner, |ui| {
        ui.horizontal_centered(|ui| {
            crate::components::avatar::avatar(
                ui,
                theme,
                user_display,
                AVATAR_RADIUS,
                false,
                avatar_texture,
            );

            ui.add_space(8.0);

            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(user_display)
                        .size(14.0)
                        .strong()
                        .color(theme.text_primary),
                );
                ui.add_space(1.0);
                ui.label(
                    egui::RichText::new(if voice.in_voice_channel {
                        "В голосовом канале"
                    } else {
                        "В сети"
                    })
                    .size(11.0)
                    .color(if voice.in_voice_channel {
                        theme.success
                    } else {
                        theme.text_muted
                    }),
                );
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let settings = icon_button(ui, theme, "⚙", "Настройки", true);
                if settings.clicked() {
                    (*on_action)(BottomPanelAction::OpenSettings);
                }

                let output = icon_button(
                    ui,
                    theme,
                    "🎧",
                    if voice.output_muted {
                        "Включить звук"
                    } else {
                        "Выключить звук"
                    },
                    true,
                );
                if output.clicked() {
                    (*on_action)(BottomPanelAction::SetOutputMuted(!voice.output_muted));
                }

                let mic = icon_button(
                    ui,
                    theme,
                    "🎤",
                    if voice.mic_muted {
                        "Включить микрофон"
                    } else {
                        "Выключить микрофон"
                    },
                    true,
                );
                if mic.clicked() {
                    (*on_action)(BottomPanelAction::SetMicMuted(!voice.mic_muted));
                }
            });
        });
    });
}

fn icon_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: &str,
    tooltip: &str,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(
            egui::RichText::new(icon)
                .size(15.0)
                .color(theme.text_secondary),
        )
        .frame(false)
        .min_size(egui::vec2(CONTROL_SIZE, CONTROL_SIZE)),
    )
    .on_hover_text(tooltip)
}
