//! Unified bottom user strip for the left side of the app.

use eframe::egui;

use crate::theme::Theme;
use crate::voice::ScreenPreset;

const AVATAR_RADIUS: f32 = 16.0;
const CONTROL_SIZE: f32 = 28.0;
const BASE_ROW_HEIGHT: f32 = 34.0;
const VOICE_ROW_HEIGHT: f32 = 32.0;
const ROW_GAP: f32 = 15.0;
const BASE_PANEL_HEIGHT: f32 = 58.0;
const VOICE_EXTRA_HEIGHT: f32 = 44.0;
const STREAM_BUTTON_GAP: f32 = 4.0;
const DISCONNECT_GAP: f32 = 4.0;

pub fn panel_height(in_voice_channel: bool) -> f32 {
    if in_voice_channel {
        BASE_PANEL_HEIGHT + VOICE_EXTRA_HEIGHT
    } else {
        BASE_PANEL_HEIGHT
    }
}

#[derive(Debug, Clone)]
pub enum BottomPanelAction {
    SetMicMuted(bool),
    SetDeafened(bool),
    OpenSettings,
    OpenStreamPicker,
    StopStream,
    SetScreenPreset(ScreenPreset),
    LeaveVoice,
}

#[derive(Clone, Default)]
pub struct BottomPanelVoiceSnapshot {
    pub in_voice_channel: bool,
    pub mic_muted: bool,
    pub output_muted: bool,
    pub screen_on: bool,
    pub screen_preset: ScreenPreset,
    pub speaking: bool,
}

pub struct BottomPanelParams<'a> {
    pub theme: &'a Theme,
    pub user_display: &'a str,
    pub voice: BottomPanelVoiceSnapshot,
    pub avatar_texture: Option<&'a egui::TextureHandle>,
    pub on_action: &'a mut dyn FnMut(BottomPanelAction),
}

pub fn show(ui: &mut egui::Ui, params: BottomPanelParams<'_>) {
    let BottomPanelParams {
        theme,
        user_display,
        voice,
        avatar_texture,
        on_action,
    } = params;

    let (rect, _) = ui.allocate_exact_size(ui.available_size_before_wrap(), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::Rounding::ZERO, theme.bg_quaternary);

    let content_rect = rect.shrink2(egui::vec2(12.0, 8.0));
    let base_row_rect = egui::Rect::from_min_size(
        egui::pos2(content_rect.left(), content_rect.bottom() - BASE_ROW_HEIGHT),
        egui::vec2(content_rect.width(), BASE_ROW_HEIGHT),
    );

    if voice.in_voice_channel {
        let voice_row_rect = egui::Rect::from_min_size(
            egui::pos2(
                content_rect.left(),
                base_row_rect.top() - ROW_GAP - VOICE_ROW_HEIGHT,
            ),
            egui::vec2(content_rect.width(), VOICE_ROW_HEIGHT),
        );
        ui.allocate_ui_at_rect(voice_row_rect, |ui| {
            ui.set_width(voice_row_rect.width());
            voice_controls_row(ui, theme, &voice, on_action);
        });
    }

    ui.allocate_ui_at_rect(base_row_rect, |ui| {
        ui.set_width(base_row_rect.width());
        base_user_row(ui, theme, user_display, &voice, avatar_texture, on_action);
    });
}

fn base_user_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    user_display: &str,
    voice: &BottomPanelVoiceSnapshot,
    avatar_texture: Option<&egui::TextureHandle>,
    on_action: &mut dyn FnMut(BottomPanelAction),
) {
    ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);

    ui.horizontal(|ui| {
        crate::components::avatar::avatar(
            ui,
            theme,
            user_display,
            AVATAR_RADIUS,
            voice.speaking,
            avatar_texture,
        );

        ui.add_space(8.0);

        let right_controls_width = CONTROL_SIZE * 3.0 + 16.0;
        let details_width = (ui.available_width() - right_controls_width).max(72.0);
        ui.allocate_ui_with_layout(
            egui::vec2(details_width, BASE_ROW_HEIGHT),
            egui::Layout::top_down_justified(egui::Align::LEFT),
            |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(
                    egui::RichText::new(user_display)
                        .size(14.0)
                        .strong()
                        .color(theme.text_primary),
                );
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
            },
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let settings = icon_button(ui, theme, "⚙", "Настройки", false);
            if settings.clicked() {
                on_action(BottomPanelAction::OpenSettings);
            }

            let deafened = voice.mic_muted && voice.output_muted;
            let deafen = icon_button(
                ui,
                theme,
                "🎧",
                if deafened {
                    "Включить микрофон и звук"
                } else {
                    "Выключить микрофон и звук"
                },
                deafened,
            );
            if deafen.clicked() {
                on_action(BottomPanelAction::SetDeafened(!deafened));
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
                voice.mic_muted,
            );
            if mic.clicked() {
                on_action(BottomPanelAction::SetMicMuted(!voice.mic_muted));
            }
        });
    });
}

fn voice_controls_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    voice: &BottomPanelVoiceSnapshot,
    on_action: &mut dyn FnMut(BottomPanelAction),
) {
    let disconnect_width = 38.0;
    let preset_width = 102.0;
    let main_width = (ui.available_width()
        - preset_width
        - disconnect_width
        - STREAM_BUTTON_GAP
        - DISCONNECT_GAP)
        .max(92.0);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = STREAM_BUTTON_GAP;

        let stream_label = if voice.screen_on {
            "Остановить трансляцию"
        } else {
            "Начать трансляцию"
        };
        let stream_response = ui.add_sized(
            egui::vec2(main_width, VOICE_ROW_HEIGHT),
            egui::Button::new(
                egui::RichText::new(stream_label)
                    .size(12.5)
                    .color(theme.text_primary),
            )
            .fill(if voice.screen_on {
                theme.success
            } else {
                theme.accent
            })
            .stroke(egui::Stroke::NONE)
            .rounding(egui::Rounding {
                nw: 6.0,
                ne: 0.0,
                sw: 6.0,
                se: 0.0,
            }),
        );
        if stream_response.clicked() {
            if voice.screen_on {
                on_action(BottomPanelAction::StopStream);
            } else {
                on_action(BottomPanelAction::OpenStreamPicker);
            }
        }

        let preset_popup_id = ui.make_persistent_id("bottom_stream_preset_popup");
        let preset_response = ui
            .add_sized(
                egui::vec2(preset_width, VOICE_ROW_HEIGHT),
                egui::Button::new(
                    egui::RichText::new(preset_button_label(voice.screen_preset))
                        .size(11.0)
                        .color(theme.text_primary),
                )
                .fill(if voice.screen_on {
                    theme.success
                } else {
                    theme.accent
                })
                .stroke(egui::Stroke::NONE)
                .rounding(egui::Rounding {
                    nw: 0.0,
                    ne: 6.0,
                    sw: 0.0,
                    se: 6.0,
                }),
            )
            .on_hover_text("Выбрать пресет трансляции");
        if preset_response.clicked() {
            ui.memory_mut(|mem| mem.toggle_popup(preset_popup_id));
        }
        egui::popup::popup_below_widget(
            ui,
            preset_popup_id,
            &preset_response,
            egui::popup::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                ui.set_min_width(preset_width.max(preset_response.rect.width()));
                for &preset in ScreenPreset::ALL {
                    if ui
                        .selectable_label(preset == voice.screen_preset, preset.label())
                        .clicked()
                    {
                        on_action(BottomPanelAction::SetScreenPreset(preset));
                        ui.memory_mut(|mem| mem.close_popup());
                    }
                }
            },
        );

        ui.add_space(DISCONNECT_GAP - STREAM_BUTTON_GAP);

        let leave = ui.add_sized(
            egui::vec2(disconnect_width, VOICE_ROW_HEIGHT),
            egui::Button::new(
                egui::RichText::new("📞")
                    .size(16.0)
                    .color(theme.text_primary),
            )
            .fill(theme.error)
            .stroke(egui::Stroke::NONE)
            .rounding(egui::Rounding::same(6.0)),
        );
        if leave.clicked() {
            on_action(BottomPanelAction::LeaveVoice);
        }
    });
}

fn icon_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: &str,
    tooltip: &str,
    active: bool,
) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(icon).size(15.0).color(if active {
            theme.text_primary
        } else {
            theme.text_secondary
        }))
        .frame(false)
        .fill(if active {
            theme.error
        } else {
            egui::Color32::TRANSPARENT
        })
        .min_size(egui::vec2(CONTROL_SIZE, CONTROL_SIZE)),
    )
    .on_hover_text(tooltip)
}

fn preset_button_label(preset: ScreenPreset) -> &'static str {
    match preset {
        ScreenPreset::P720F30 => "720p 30fps",
        ScreenPreset::P720F60 => "720p 60fps",
        ScreenPreset::P720F120 => "720p 120fps",
        ScreenPreset::P1080F30 => "1080p 30fps",
        ScreenPreset::P1080F60 => "1080p 60fps",
        ScreenPreset::P1080F120 => "1080p 120fps",
        ScreenPreset::P1440F30 => "1440p 30fps",
        ScreenPreset::P1440F60 => "1440p 60fps",
        ScreenPreset::P1440F90 => "1440p 90fps",
    }
}
