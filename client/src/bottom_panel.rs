//! Unified bottom user strip for the left side of the app.

use eframe::egui;

use crate::theme::Theme;
use crate::voice::ScreenPreset;

const AVATAR_RADIUS: f32 = 16.0;
const CONTROL_SIZE: f32 = 28.0;
const LATENCY_BUTTON_SIZE: egui::Vec2 = egui::vec2(28.0, 24.0);
const BASE_ROW_HEIGHT: f32 = 34.0;
const VOICE_ROW_HEIGHT: f32 = 32.0;
const LATENCY_ROW_HEIGHT: f32 = 34.0;
const CONTROLS_TO_LATENCY_GAP: f32 = 8.0;
const LATENCY_TO_BASE_GAP: f32 = 8.0;
pub(crate) const BASE_PANEL_HEIGHT: f32 = 58.0;
const VOICE_EXTRA_HEIGHT: f32 =
    VOICE_ROW_HEIGHT + LATENCY_ROW_HEIGHT + CONTROLS_TO_LATENCY_GAP + LATENCY_TO_BASE_GAP;
const STREAM_BUTTON_GAP: f32 = 4.0;
const LATENCY_GRAPH_HEIGHT: f32 = 84.0;
const STREAM_BUTTON_WIDTH: f32 = 42.0;
const PRESET_BUTTON_WIDTH: f32 = 82.0;
const STREAM_AUDIO_BUTTON_WIDTH: f32 = 32.0;
const DISCONNECT_BUTTON_WIDTH: f32 = 38.0;
const LATENCY_LABEL_GAP: f32 = 8.0;
const SCREEN_SHARE_TOOLTIP_START: &str =
    "\u{41d}\u{430}\u{447}\u{430}\u{442}\u{44c} \u{442}\u{440}\u{430}\u{43d}\u{441}\u{43b}\u{44f}\u{446}\u{438}\u{44e}";
const SCREEN_SHARE_TOOLTIP_STOP: &str =
    "\u{41e}\u{441}\u{442}\u{430}\u{43d}\u{43e}\u{432}\u{438}\u{442}\u{44c} \u{442}\u{440}\u{430}\u{43d}\u{441}\u{43b}\u{44f}\u{446}\u{438}\u{44e}";

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
    SetScreenAudioMuted(bool),
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
    pub latency_ms: Option<f32>,
    pub latency_history_ms: Vec<f32>,
    pub screen_audio_muted: bool,
    pub voice_connection_label: Option<String>,
    pub stream_fps: Option<f32>,
    pub resolution: Option<(u32, u32)>,
    pub outgoing_speed_mbps: Option<f32>,
    pub webrtc_requested_bitrate_mbps: Option<f32>,
    pub webrtc_target_bitrate_mbps: Option<f32>,
    pub webrtc_fps_hint: Option<f32>,
    pub encoded_pre_rtp_bitrate_mbps: Option<f32>,
    pub source_fps: Option<f32>,
    pub source_cap_fps: Option<f32>,
    pub webrtc_effective_fps_cap: Option<f32>,
    pub startup_transport_cap_fps: Option<f32>,
    pub final_schedule_cap_fps: Option<f32>,
    pub webrtc_available_outgoing_bitrate_mbps: Option<f32>,
    pub webrtc_packet_loss_pct: Option<f32>,
    pub webrtc_nack_count: Option<u32>,
    pub webrtc_pli_count: Option<u32>,
    pub webrtc_quality_limitation_reason: Option<String>,
    pub webrtc_transport_path: Option<String>,
    pub webrtc_transport_rtt_ms: Option<f32>,
    pub encoding_path: Option<String>,
    pub decoding_path: Option<String>,
}

pub struct BottomPanelParams<'a> {
    pub theme: &'a Theme,
    pub user_display: &'a str,
    pub user_id: Option<i64>,
    pub voice: BottomPanelVoiceSnapshot,
    pub avatar_texture: Option<&'a egui::TextureHandle>,
    pub on_action: &'a mut dyn FnMut(BottomPanelAction),
}

pub fn show(ui: &mut egui::Ui, params: BottomPanelParams<'_>) {
    let BottomPanelParams {
        theme,
        user_display,
        user_id,
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

    ui.allocate_ui_at_rect(base_row_rect, |ui| {
        ui.set_width(base_row_rect.width());
        base_user_row(ui, theme, user_display, user_id, &voice, avatar_texture, on_action);
    });

    if voice.in_voice_channel {
        let latency_row_rect = egui::Rect::from_min_size(
            egui::pos2(
                content_rect.left(),
                base_row_rect.top() - LATENCY_TO_BASE_GAP - LATENCY_ROW_HEIGHT,
            ),
            egui::vec2(content_rect.width(), LATENCY_ROW_HEIGHT),
        );
        let voice_row_rect = egui::Rect::from_min_size(
            egui::pos2(
                content_rect.left(),
                latency_row_rect.top() - CONTROLS_TO_LATENCY_GAP - VOICE_ROW_HEIGHT,
            ),
            egui::vec2(content_rect.width(), VOICE_ROW_HEIGHT),
        );

        ui.allocate_ui_at_rect(voice_row_rect, |ui| {
            ui.set_width(voice_row_rect.width());
            voice_controls_row(ui, theme, &voice, on_action);
        });
        ui.allocate_ui_at_rect(latency_row_rect, |ui| {
            ui.set_width(latency_row_rect.width());
            voice_latency_row(ui, theme, &voice);
        });
    }
}

fn base_user_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    user_display: &str,
    user_id: Option<i64>,
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
                if let Some(uid) = user_id {
                    ui.label(
                        egui::RichText::new(uid.to_string())
                            .size(11.0)
                            .color(theme.text_muted),
                    );
                } else {
                    ui.label(
                        egui::RichText::new("В сети")
                            .size(11.0)
                            .color(theme.text_muted),
                    );
                }
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
    let row_rect = ui.max_rect();
    let stream_rect = egui::Rect::from_min_size(
        row_rect.min,
        egui::vec2(STREAM_BUTTON_WIDTH, VOICE_ROW_HEIGHT),
    );
    let preset_rect = egui::Rect::from_min_size(
        egui::pos2(stream_rect.right() + STREAM_BUTTON_GAP, row_rect.top()),
        egui::vec2(PRESET_BUTTON_WIDTH, VOICE_ROW_HEIGHT),
    );
    let stream_audio_rect = egui::Rect::from_min_size(
        egui::pos2(preset_rect.right() + STREAM_BUTTON_GAP, row_rect.top()),
        egui::vec2(STREAM_AUDIO_BUTTON_WIDTH, VOICE_ROW_HEIGHT),
    );
    let leave_rect = egui::Rect::from_min_size(
        egui::pos2(row_rect.right() - DISCONNECT_BUTTON_WIDTH, row_rect.top()),
        egui::vec2(DISCONNECT_BUTTON_WIDTH, VOICE_ROW_HEIGHT),
    );

    let stream_response = ui
        .put(
            stream_rect,
            egui::Button::new("")
                .min_size(stream_rect.size())
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
        )
        .on_hover_text(if voice.screen_on {
            SCREEN_SHARE_TOOLTIP_STOP
        } else {
            SCREEN_SHARE_TOOLTIP_START
        });
    paint_stream_icon(ui.painter(), stream_response.rect, theme.text_primary);
    if stream_response.clicked() {
        if voice.screen_on {
            on_action(BottomPanelAction::StopStream);
        } else {
            on_action(BottomPanelAction::OpenStreamPicker);
        }
    }

    let preset_popup_id = ui.make_persistent_id("bottom_stream_preset_popup");
    let preset_response = ui
        .put(
            preset_rect,
            egui::Button::new(
                egui::RichText::new(preset_button_label(voice.screen_preset))
                    .size(10.0)
                    .color(theme.text_primary),
            )
            .min_size(preset_rect.size())
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
            ui.set_min_width(PRESET_BUTTON_WIDTH.max(preset_response.rect.width()));
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

    let stream_audio_response = ui
        .put(
            stream_audio_rect,
            egui::Button::new("")
                .min_size(stream_audio_rect.size())
                .fill(if voice.screen_audio_muted {
                    theme.error
                } else {
                    theme.success
                })
                .stroke(egui::Stroke::NONE)
                .rounding(egui::Rounding::same(6.0)),
        )
        .on_hover_text(if voice.screen_audio_muted {
            "Включить звук трансляции"
        } else {
            "Выключить звук трансляции"
        });
    paint_stream_audio_icon(
        ui.painter(),
        stream_audio_response.rect,
        theme.text_primary,
        voice.screen_audio_muted,
    );
    if stream_audio_response.clicked() {
        on_action(BottomPanelAction::SetScreenAudioMuted(
            !voice.screen_audio_muted,
        ));
    }

    let leave = ui.put(
        leave_rect,
        egui::Button::new(
            egui::RichText::new("📞")
                .size(16.0)
                .color(theme.text_primary),
        )
        .min_size(leave_rect.size())
        .fill(theme.error)
        .stroke(egui::Stroke::NONE)
        .rounding(egui::Rounding::same(6.0)),
    );
    if leave.clicked() {
        on_action(BottomPanelAction::LeaveVoice);
    }
}

fn voice_latency_row(ui: &mut egui::Ui, theme: &Theme, voice: &BottomPanelVoiceSnapshot) {
    let row_rect = ui.max_rect();
    let popup_id = ui.make_persistent_id("bottom_voice_latency_popup");
    let button_rect = egui::Rect::from_min_size(
        egui::pos2(
            row_rect.left(),
            row_rect.center().y - LATENCY_BUTTON_SIZE.y * 0.5,
        ),
        LATENCY_BUTTON_SIZE,
    );
    let indicator_color = latency_indicator_color(theme, voice.latency_ms);
    let tooltip = voice
        .latency_ms
        .map(|latency| format!("RTT до сервера: {:.0} мс", latency))
        .unwrap_or_else(|| "Показать задержку соединения".to_string());
    let response = ui
        .put(
            button_rect,
            egui::Button::new("")
                .min_size(button_rect.size())
                .fill(theme.bg_tertiary)
                .stroke(egui::Stroke::NONE)
                .rounding(egui::Rounding::same(6.0)),
        )
        .on_hover_text(tooltip);
    paint_wifi_icon(
        ui.painter(),
        response.rect.shrink2(egui::vec2(5.0, 3.0)),
        indicator_color,
    );
    if response.clicked() {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }

    let text_rect = egui::Rect::from_min_max(
        egui::pos2(button_rect.right() + LATENCY_LABEL_GAP, row_rect.top()),
        egui::pos2(row_rect.right(), row_rect.bottom()),
    );
    let connection_label = voice
        .voice_connection_label
        .as_deref()
        .unwrap_or("Сервер / Голосовой канал");
    ui.allocate_ui_at_rect(text_rect, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
            ui.label(
                egui::RichText::new("В голосовом канале")
                    .size(12.0)
                    .color(theme.text_primary),
            );
            ui.label(
                egui::RichText::new(connection_label)
                    .size(10.5)
                    .color(theme.text_muted),
            );
        });
    });

    egui::popup::popup_below_widget(
        ui,
        popup_id,
        &response,
        egui::popup::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            latency_popup_contents(ui, theme, voice);
        },
    );
}

fn latency_popup_contents(ui: &mut egui::Ui, theme: &Theme, voice: &BottomPanelVoiceSnapshot) {
    ui.set_min_width(290.0);
    ui.label(
        egui::RichText::new("Задержка соединения")
            .size(13.0)
            .strong()
            .color(theme.text_primary),
    );
    ui.add_space(6.0);

    if let Some(latency_ms) = voice.latency_ms {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("RTT:")
                    .size(12.0)
                    .color(theme.text_muted),
            );
            ui.label(
                egui::RichText::new(format!("{:.0} мс", latency_ms))
                    .size(18.0)
                    .strong()
                    .color(latency_indicator_color(theme, Some(latency_ms))),
            );
        });
    } else {
        ui.label(
            egui::RichText::new("Собираем данные о задержке...")
                .size(12.0)
                .color(theme.text_muted),
        );
    }

    if let Some((min_ms, max_ms)) = latency_min_max(&voice.latency_history_ms) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("Мин: {:.0} мс", min_ms))
                    .size(11.0)
                    .color(theme.text_muted),
            );
            ui.label(
                egui::RichText::new(format!("Макс: {:.0} мс", max_ms))
                    .size(11.0)
                    .color(theme.text_muted),
            );
        });
    }

    ui.add_space(8.0);
    latency_graph(ui, theme, &voice.latency_history_ms, voice.latency_ms);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("5 минут назад")
                .size(10.0)
                .color(theme.text_muted),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new("Сейчас")
                    .size(10.0)
                    .color(theme.text_muted),
            );
        });
    });

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(8.0);

    let (_, _, preset_fps, preset_bitrate_bps) = voice.screen_preset.params();

    popup_stat_row(
        ui,
        theme,
        "Preset",
        format!(
            "{} / {:.0} fps / {:.1} Mbps",
            voice.screen_preset.label(),
            preset_fps,
            preset_bitrate_bps as f32 / 1_000_000.0
        ),
    );
    popup_stat_row(
        ui,
        theme,
        "Source FPS",
        voice
            .source_fps
            .map(|fps| format!("{fps:.1}"))
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Caps",
        format!(
            "src {} / bwe {} / boot {} / final {}",
            voice
                .source_cap_fps
                .map(|fps| format!("{fps:.0}"))
                .unwrap_or_else(|| "-".to_string()),
            voice
                .webrtc_effective_fps_cap
                .map(|fps| format!("{fps:.0}"))
                .unwrap_or_else(|| "-".to_string()),
            voice
                .startup_transport_cap_fps
                .map(|fps| format!("{fps:.0}"))
                .unwrap_or_else(|| "-".to_string()),
            voice
                .final_schedule_cap_fps
                .map(|fps| format!("{fps:.0}"))
                .unwrap_or_else(|| "-".to_string()),
        ),
    );
    popup_stat_row(
        ui,
        theme,
        "BWE target",
        voice
            .webrtc_requested_bitrate_mbps
            .map(|speed| format!("{speed:.2} Mbps"))
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Encoder bitrate",
        voice
            .webrtc_target_bitrate_mbps
            .map(|speed| format!("{speed:.2} Mbps"))
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "FPS hint",
        voice
            .webrtc_fps_hint
            .map(|fps| format!("{fps:.1}"))
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Available bitrate",
        voice
            .webrtc_available_outgoing_bitrate_mbps
            .map(|speed| format!("{speed:.2} Mbps"))
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Pre-RTP bitrate",
        voice
            .encoded_pre_rtp_bitrate_mbps
            .map(|speed| format!("{speed:.2} Mbps"))
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Packet loss",
        voice
            .webrtc_packet_loss_pct
            .map(|loss| format!("{loss:.2}%"))
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "NACK",
        voice
            .webrtc_nack_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "PLI",
        voice
            .webrtc_pli_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Quality limit",
        voice
            .webrtc_quality_limitation_reason
            .clone()
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Transport path",
        voice
            .webrtc_transport_path
            .clone()
            .unwrap_or_else(|| "-".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Transport RTT",
        voice
            .webrtc_transport_rtt_ms
            .map(|rtt| format!("{rtt:.1} ms"))
            .unwrap_or_else(|| "-".to_string()),
    );

    popup_stat_row(
        ui,
        theme,
        "ФПС трансляции",
        voice
            .stream_fps
            .map(|fps| format!("{fps:.1}"))
            .unwrap_or_else(|| "—".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Разрешение",
        voice
            .resolution
            .map(|(width, height)| format!("{width}×{height}"))
            .unwrap_or_else(|| "—".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Исходящая скорость",
        voice
            .outgoing_speed_mbps
            .map(|speed| format!("{speed:.2} Мбит/с"))
            .unwrap_or_else(|| "—".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Кодирование",
        voice
            .encoding_path
            .clone()
            .unwrap_or_else(|| "—".to_string()),
    );
    popup_stat_row(
        ui,
        theme,
        "Декодирование",
        voice
            .decoding_path
            .clone()
            .unwrap_or_else(|| "—".to_string()),
    );
}

fn latency_graph(
    ui: &mut egui::Ui,
    theme: &Theme,
    latency_history_ms: &[f32],
    current_latency_ms: Option<f32>,
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), LATENCY_GRAPH_HEIGHT),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::Rounding::same(6.0), theme.bg_tertiary);

    if latency_history_ms.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Нет данных",
            egui::FontId::proportional(12.0),
            theme.text_muted,
        );
        return;
    }

    let plot_rect = rect.shrink2(egui::vec2(10.0, 10.0));
    let min_ms = latency_history_ms
        .iter()
        .copied()
        .fold(f32::INFINITY, f32::min);
    let max_ms = latency_history_ms
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);
    let padding = ((max_ms - min_ms) * 0.15).max(4.0);
    let lower = (min_ms - padding).max(0.0);
    let upper = (max_ms + padding).max(lower + 1.0);
    let range = upper - lower;

    for step in 0..=2 {
        let t = step as f32 / 2.0;
        let y = egui::lerp(plot_rect.bottom()..=plot_rect.top(), t);
        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), y),
                egui::pos2(plot_rect.right(), y),
            ],
            egui::Stroke::new(
                1.0,
                Theme::lerp_color(theme.border, theme.bg_quaternary, 0.45),
            ),
        );
    }

    let stroke = egui::Stroke::new(1.6, latency_indicator_color(theme, current_latency_ms));
    if latency_history_ms.len() == 1 {
        let y = latency_to_plot_y(latency_history_ms[0], plot_rect, lower, range);
        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), y),
                egui::pos2(plot_rect.right(), y),
            ],
            stroke,
        );
        painter.circle_filled(egui::pos2(plot_rect.right(), y), 2.6, stroke.color);
        return;
    }

    let points: Vec<egui::Pos2> = latency_history_ms
        .iter()
        .enumerate()
        .map(|(index, latency_ms)| {
            let t = index as f32 / (latency_history_ms.len().saturating_sub(1)) as f32;
            egui::pos2(
                egui::lerp(plot_rect.left()..=plot_rect.right(), t),
                latency_to_plot_y(*latency_ms, plot_rect, lower, range),
            )
        })
        .collect();
    painter.add(egui::Shape::line(points.clone(), stroke));
    if let Some(last) = points.last() {
        painter.circle_filled(*last, 2.6, stroke.color);
    }
}

fn latency_to_plot_y(latency_ms: f32, plot_rect: egui::Rect, lower: f32, range: f32) -> f32 {
    let normalized = ((latency_ms - lower) / range).clamp(0.0, 1.0);
    egui::lerp(plot_rect.bottom()..=plot_rect.top(), normalized)
}

fn latency_min_max(latency_history_ms: &[f32]) -> Option<(f32, f32)> {
    let min_ms = latency_history_ms.iter().copied().reduce(f32::min)?;
    let max_ms = latency_history_ms.iter().copied().reduce(f32::max)?;
    Some((min_ms, max_ms))
}

fn latency_indicator_color(theme: &Theme, latency_ms: Option<f32>) -> egui::Color32 {
    match latency_ms {
        Some(latency) if latency <= 50.0 => theme.success,
        Some(latency) if latency < 150.0 => theme.warning,
        Some(_) => theme.error,
        None => theme.text_muted,
    }
}

fn paint_wifi_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let center = egui::pos2(rect.center().x, rect.bottom() - 5.0);
    let stroke = egui::Stroke::new(1.4, color);
    for radius in [4.0, 7.5, 11.0] {
        painter.add(egui::Shape::line(
            wifi_arc_points(center, radius, 10),
            stroke,
        ));
    }
    painter.circle_filled(egui::pos2(center.x, center.y + 2.0), 1.8, color);
}

fn paint_stream_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let icon_rect = rect.shrink2(egui::vec2(8.0, 7.0));
    let screen_rect = egui::Rect::from_min_max(
        egui::pos2(icon_rect.left(), icon_rect.top() + 1.0),
        egui::pos2(icon_rect.left() + 13.0, icon_rect.top() + 10.0),
    );
    let stroke = egui::Stroke::new(1.35, color);

    painter.rect_stroke(screen_rect, egui::Rounding::same(1.5), stroke);
    painter.line_segment(
        [
            egui::pos2(screen_rect.center().x, screen_rect.bottom()),
            egui::pos2(screen_rect.center().x, screen_rect.bottom() + 3.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(screen_rect.left() + 2.0, screen_rect.bottom() + 3.6),
            egui::pos2(screen_rect.right() - 2.0, screen_rect.bottom() + 3.6),
        ],
        stroke,
    );

    let arrow_y = screen_rect.center().y;
    let arrow_start = egui::pos2(screen_rect.right() + 2.5, arrow_y);
    let arrow_end = egui::pos2(icon_rect.right(), arrow_y);
    painter.line_segment([arrow_start, arrow_end], stroke);
    painter.line_segment(
        [egui::pos2(arrow_end.x - 3.2, arrow_y - 2.5), arrow_end],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(arrow_end.x - 3.2, arrow_y + 2.5), arrow_end],
        stroke,
    );
}

fn paint_stream_audio_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    muted: bool,
) {
    let stroke = egui::Stroke::new(1.35, color);
    let icon_rect = rect.shrink2(egui::vec2(8.0, 7.0));

    let speaker_points = vec![
        egui::pos2(icon_rect.left(), icon_rect.center().y - 2.0),
        egui::pos2(icon_rect.left() + 3.0, icon_rect.center().y - 2.0),
        egui::pos2(icon_rect.left() + 6.5, icon_rect.top()),
        egui::pos2(icon_rect.left() + 6.5, icon_rect.bottom()),
        egui::pos2(icon_rect.left() + 3.0, icon_rect.center().y + 2.0),
        egui::pos2(icon_rect.left(), icon_rect.center().y + 2.0),
    ];
    painter.add(egui::Shape::closed_line(speaker_points, stroke));

    if muted {
        painter.line_segment(
            [
                egui::pos2(icon_rect.left() + 9.0, icon_rect.top() + 0.5),
                egui::pos2(icon_rect.right(), icon_rect.bottom() - 0.5),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(icon_rect.left() + 9.0, icon_rect.bottom() - 0.5),
                egui::pos2(icon_rect.right(), icon_rect.top() + 0.5),
            ],
            stroke,
        );
    } else {
        let center = egui::pos2(icon_rect.left() + 8.2, icon_rect.center().y);
        for radius in [3.0, 5.5] {
            painter.add(egui::Shape::line(
                speaker_wave_points(center, radius, 8),
                stroke,
            ));
        }
    }
}

fn wifi_arc_points(center: egui::Pos2, radius: f32, segments: usize) -> Vec<egui::Pos2> {
    let start_angle = std::f32::consts::PI * 1.20;
    let end_angle = std::f32::consts::PI * 1.80;
    (0..=segments)
        .map(|step| {
            let t = step as f32 / segments as f32;
            let angle = egui::lerp(start_angle..=end_angle, t);
            egui::pos2(
                center.x + angle.cos() * radius,
                center.y + angle.sin() * radius,
            )
        })
        .collect()
}

fn speaker_wave_points(center: egui::Pos2, radius: f32, segments: usize) -> Vec<egui::Pos2> {
    let start_angle = -0.65;
    let end_angle = 0.65;
    (0..=segments)
        .map(|step| {
            let t = step as f32 / segments as f32;
            let angle = egui::lerp(start_angle..=end_angle, t);
            egui::pos2(
                center.x + angle.cos() * radius,
                center.y + angle.sin() * radius,
            )
        })
        .collect()
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
        ScreenPreset::P720F30 => "720p30",
        ScreenPreset::P720F60 => "720p60",
        ScreenPreset::P720F120 => "720p120",
        ScreenPreset::P1080F30 => "1080p30",
        ScreenPreset::P1080F60 => "1080p60",
        ScreenPreset::P1080F120 => "1080p120",
        ScreenPreset::P1440F30 => "1440p30",
        ScreenPreset::P1440F60 => "1440p60",
        ScreenPreset::P1440F90 => "1440p90",
    }
}

fn popup_stat_row(ui: &mut egui::Ui, theme: &Theme, label: &str, value: String) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(format!("{label}:"))
                .size(11.0)
                .color(theme.text_muted),
        );
        ui.label(
            egui::RichText::new(value)
                .size(11.0)
                .color(theme.text_primary),
        );
    });
}
