//! Unified bottom user strip for the left side of the app.

use eframe::egui;

use crate::theme::Theme;
use crate::voice::ScreenPreset;

const AVATAR_RADIUS: f32 = 16.0;
const CONTROL_SIZE: f32 = 28.0;
const LATENCY_BUTTON_SIZE: egui::Vec2 = egui::vec2(28.0, 24.0);
const BASE_ROW_HEIGHT: f32 = 34.0;
const VOICE_ROW_HEIGHT: f32 = 32.0;
const LATENCY_ROW_HEIGHT: f32 = 24.0;
const CONTROLS_TO_LATENCY_GAP: f32 = 8.0;
const LATENCY_TO_BASE_GAP: f32 = 8.0;
const BASE_PANEL_HEIGHT: f32 = 58.0;
const VOICE_EXTRA_HEIGHT: f32 =
    VOICE_ROW_HEIGHT + LATENCY_ROW_HEIGHT + CONTROLS_TO_LATENCY_GAP + LATENCY_TO_BASE_GAP;
const STREAM_BUTTON_GAP: f32 = 4.0;
const DISCONNECT_GAP: f32 = 4.0;
const LATENCY_GRAPH_HEIGHT: f32 = 84.0;

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
    pub latency_ms: Option<f32>,
    pub latency_history_ms: Vec<f32>,
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
    let row_rect = ui.max_rect();
    let disconnect_width = 38.0;
    let preset_width = 102.0;
    let main_width = (row_rect.width()
        - preset_width
        - disconnect_width
        - STREAM_BUTTON_GAP
        - DISCONNECT_GAP)
        .max(92.0);
    let stream_rect = egui::Rect::from_min_size(
        row_rect.min,
        egui::vec2(main_width, VOICE_ROW_HEIGHT),
    );
    let preset_rect = egui::Rect::from_min_size(
        egui::pos2(stream_rect.right() + STREAM_BUTTON_GAP, row_rect.top()),
        egui::vec2(preset_width, VOICE_ROW_HEIGHT),
    );
    let leave_rect = egui::Rect::from_min_size(
        egui::pos2(preset_rect.right() + DISCONNECT_GAP, row_rect.top()),
        egui::vec2(disconnect_width, VOICE_ROW_HEIGHT),
    );

    let stream_label = if voice.screen_on {
        "Остановить трансляцию"
    } else {
        "Начать трансляцию"
    };
    let stream_response = ui.put(
        stream_rect,
        egui::Button::new(
            egui::RichText::new(stream_label)
                .size(12.5)
                .color(theme.text_primary),
        )
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
        .put(
            preset_rect,
            egui::Button::new(
                egui::RichText::new(preset_button_label(voice.screen_preset))
                    .size(11.0)
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
            row_rect.right() - LATENCY_BUTTON_SIZE.x,
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
    paint_wifi_icon(ui.painter(), response.rect.shrink2(egui::vec2(5.0, 3.0)), indicator_color);
    if response.clicked() {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
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

fn latency_popup_contents(
    ui: &mut egui::Ui,
    theme: &Theme,
    voice: &BottomPanelVoiceSnapshot,
) {
    ui.set_min_width(220.0);
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
            egui::Stroke::new(1.0, Theme::lerp_color(theme.border, theme.bg_quaternary, 0.45)),
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
        Some(latency) if latency < 80.0 => theme.success,
        Some(latency) if latency < 140.0 => theme.warning,
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
