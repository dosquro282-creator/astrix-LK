//! Left sidebar with channel lists and the shared left-side user bar.

use std::collections::{HashMap, HashSet};

use eframe::egui;

use crate::net::VoiceParticipant;
use crate::theme::Theme;

pub const CHANNEL_PANEL_WIDTH: f32 = 240.0;

const SERVER_HEADER_HEIGHT: f32 = 48.0;
const HEADER_BUTTON_SIZE: egui::Vec2 = egui::vec2(26.0, 26.0);
const CHANNEL_ROW_HEIGHT: f32 = 30.0;

#[derive(Debug, Clone)]
pub enum ChannelPanelAction {
    SelectChannel(i64),
    JoinVoice { channel_id: i64, server_id: i64 },
    LeaveVoice,
    SetMicMuted(bool),
    SetOutputMuted(bool),
    SetCameraEnabled(bool),
    ToggleScreenShare,
    SetParticipantMuted { user_id: i64, muted: bool },
    SetParticipantDenoise { user_id: i64, enabled: bool },
    SetParticipantVolume { user_id: i64, volume: f32 },
    CreateChannel,
    Invite,
    ChannelSettings(i64, String),
    OpenServerSettings,
    Logout,
    RetryChannels,
}

#[derive(Clone, Default)]
pub struct ChannelPanelVoiceSnapshot {
    pub channel_id: Option<i64>,
    pub server_id: Option<i64>,
    pub mic_muted: bool,
    pub output_muted: bool,
    pub camera_on: bool,
    pub screen_on: bool,
    pub channel_voice: HashMap<i64, Vec<VoiceParticipant>>,
    pub speaking: HashMap<i64, bool>,
    pub local_volumes: HashMap<i64, f32>,
    pub locally_muted: HashSet<i64>,
    pub receiver_denoise_users: HashSet<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChannelsLoadState {
    Idle,
    Loading,
    Loaded,
    Error(String),
}

pub struct ChannelPanelParams<'a> {
    pub theme: &'a Theme,
    pub bottom_reserved_height: f32,
    pub server_name: &'a str,
    pub server_id: i64,
    pub text_channels: &'a [(i64, String)],
    pub voice_channels: &'a [(i64, String)],
    pub unread_channel_ids: &'a HashSet<i64>,
    pub selected_channel_id: Option<i64>,
    pub voice: ChannelPanelVoiceSnapshot,
    pub user_id: Option<i64>,
    pub on_action: &'a mut dyn FnMut(ChannelPanelAction),
    pub channels_load: ChannelsLoadState,
}

pub fn show(ctx: &egui::Context, ui: &mut egui::Ui, params: ChannelPanelParams<'_>) {
    let ChannelPanelParams {
        theme,
        bottom_reserved_height,
        server_name,
        server_id,
        text_channels,
        voice_channels,
        unread_channel_ids,
        selected_channel_id,
        voice,
        user_id,
        on_action,
        channels_load,
    } = params;

    ui.painter()
        .rect_filled(ui.max_rect(), egui::Rounding::ZERO, theme.bg_secondary);

    egui::TopBottomPanel::bottom("channel_panel_user")
        .exact_height(bottom_reserved_height)
        .show_separator_line(false)
        .show_inside(ui, |_ui| {});

    egui::TopBottomPanel::top("channel_panel_header")
        .exact_height(SERVER_HEADER_HEIGHT)
        .frame(egui::Frame::none().fill(theme.bg_quaternary))
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter().line_segment(
                [rect.left_bottom(), rect.right_bottom()],
                egui::Stroke::new(1.0, theme.border),
            );

            let left_padding = 10.0;
            let right_padding = 10.0;
            let button_gap = 4.0;
            let buttons_width = HEADER_BUTTON_SIZE.x * 3.0 + button_gap * 2.0;
            let buttons_rect = egui::Rect::from_min_size(
                egui::pos2(
                    rect.right() - right_padding - buttons_width,
                    rect.center().y - HEADER_BUTTON_SIZE.y * 0.5,
                ),
                egui::vec2(buttons_width, HEADER_BUTTON_SIZE.y),
            );
            let title_rect = egui::Rect::from_min_max(
                egui::pos2(rect.left() + left_padding, rect.top()),
                egui::pos2(
                    (buttons_rect.left() - 12.0).max(rect.left() + left_padding),
                    rect.bottom(),
                ),
            );

            ui.allocate_ui_at_rect(title_rect, |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(server_name)
                            .size(16.0)
                            .strong()
                            .color(theme.text_primary),
                    );
                });
            });

            ui.allocate_ui_at_rect(buttons_rect, |ui| {
                ui.spacing_mut().item_spacing.x = button_gap;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if header_button(ui, theme, "+", "Create channel").clicked() {
                        (*on_action)(ChannelPanelAction::CreateChannel);
                    }
                    if header_button(ui, theme, "@", "Invite").clicked() {
                        (*on_action)(ChannelPanelAction::Invite);
                    }
                    if header_button(ui, theme, "⚙", "Настройки сервера").clicked() {
                        (*on_action)(ChannelPanelAction::OpenServerSettings);
                    }
                });
            });
        });

    egui::ScrollArea::vertical()
        .id_source("channel_panel_scroll")
        .show(ui, |ui| match channels_load {
            ChannelsLoadState::Loading => loading_state(ui, theme, "Loading channels..."),
            ChannelsLoadState::Error(ref error) => {
                ui.add_space(24.0);
                ui.label(egui::RichText::new(error).color(theme.error));
                ui.add_space(8.0);
                if ui.button("Retry").clicked() {
                    (*on_action)(ChannelPanelAction::RetryChannels);
                }
            }
            _ => {
                ui.add_space(10.0);
                ui.add_space(10.0);
                section_label(ui, theme, "TEXT CHANNELS");
                ui.add_space(4.0);
                for (id, name) in text_channels {
                    let response = channel_row(
                        ctx,
                        ui,
                        theme,
                        ChannelRowParams {
                            row_id: format!("text_channel_{id}"),
                            icon: "#",
                            label: name.clone(),
                            active: selected_channel_id == Some(*id),
                            unread: unread_channel_ids.contains(id),
                            metadata: None,
                        },
                    );
                    if response.clicked() {
                        (*on_action)(ChannelPanelAction::SelectChannel(*id));
                    }
                    response.context_menu(|ui| {
                        if ui.button("Channel settings").clicked() {
                            (*on_action)(ChannelPanelAction::ChannelSettings(*id, name.clone()));
                            ui.close_menu();
                        }
                    });
                }

                ui.add_space(10.0);
                ui.add_space(10.0);
                section_label(ui, theme, "VOICE CHANNELS");
                ui.add_space(4.0);
                for (id, name) in voice_channels {
                    let in_this_voice = voice.channel_id == Some(*id);
                    let participant_count =
                        voice.channel_voice.get(id).map(|v| v.len()).unwrap_or(0);

                    let response = channel_row(
                        ctx,
                        ui,
                        theme,
                        ChannelRowParams {
                            row_id: format!("voice_channel_{id}"),
                            icon: "🔊",
                            label: name.clone(),
                            active: selected_channel_id == Some(*id) || in_this_voice,
                            unread: false,
                            metadata: Some(if participant_count > 0 {
                                participant_count.to_string()
                            } else {
                                String::new()
                            }),
                        },
                    );
                    if response.clicked() {
                        (*on_action)(ChannelPanelAction::SelectChannel(*id));
                        if !in_this_voice {
                            (*on_action)(ChannelPanelAction::JoinVoice {
                                channel_id: *id,
                                server_id,
                            });
                        }
                    }
                    response.context_menu(|ui| {
                        if ui.button("Channel settings").clicked() {
                            (*on_action)(ChannelPanelAction::ChannelSettings(*id, name.clone()));
                            ui.close_menu();
                        }
                    });

                    if let Some(participants) = voice.channel_voice.get(id) {
                        for participant in participants {
                            let is_locally_muted =
                                voice.locally_muted.contains(&participant.user_id);
                            let row = voice_participant_row(
                                ui,
                                theme,
                                participant,
                                *voice.speaking.get(&participant.user_id).unwrap_or(&false),
                                Some(participant.user_id) == user_id,
                                participant.deafened,
                                is_locally_muted,
                            );
                            if voice.channel_id == Some(*id) && Some(participant.user_id) != user_id
                            {
                                let receiver_denoise =
                                    voice.receiver_denoise_users.contains(&participant.user_id);
                                row.context_menu(|ui| {
                                    let mute_label = if is_locally_muted {
                                        "Снять локальный мут"
                                    } else {
                                        "Локально заглушить"
                                    };
                                    if ui.button(mute_label).clicked() {
                                        (*on_action)(ChannelPanelAction::SetParticipantMuted {
                                            user_id: participant.user_id,
                                            muted: !is_locally_muted,
                                        });
                                        ui.close_menu();
                                    }

                                    let denoise_label = if receiver_denoise {
                                        "Выключить локальное шумоподавление"
                                    } else {
                                        "Включить локальное шумоподавление"
                                    };
                                    if ui.button(denoise_label).clicked() {
                                        (*on_action)(ChannelPanelAction::SetParticipantDenoise {
                                            user_id: participant.user_id,
                                            enabled: !receiver_denoise,
                                        });
                                        ui.close_menu();
                                    }

                                    let mut volume = voice
                                        .local_volumes
                                        .get(&participant.user_id)
                                        .copied()
                                        .unwrap_or(1.0);
                                    if ui
                                        .add(
                                            egui::Slider::new(&mut volume, 0.0..=3.0)
                                                .text("Громкость")
                                                .custom_formatter(|value, _| {
                                                    format!("{:.0}%", value * 100.0)
                                                }),
                                        )
                                        .changed()
                                    {
                                        (*on_action)(ChannelPanelAction::SetParticipantVolume {
                                            user_id: participant.user_id,
                                            volume,
                                        });
                                    }
                                });
                            }
                        }
                    }
                }
            }
        });
}

fn loading_state(ui: &mut egui::Ui, theme: &Theme, label: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(32.0);
        ui.spinner();
        ui.add_space(8.0);
        ui.label(egui::RichText::new(label).color(theme.text_muted));
    });
}

fn section_label(ui: &mut egui::Ui, theme: &Theme, title: &str) {
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(title)
                .size(11.0)
                .strong()
                .color(theme.text_muted),
        );
    });
}

fn header_button(ui: &mut egui::Ui, theme: &Theme, label: &str, tooltip: &str) -> egui::Response {
    ui.add_sized(
        HEADER_BUTTON_SIZE,
        egui::Button::new(
            egui::RichText::new(label)
                .size(13.0)
                .color(theme.text_secondary),
        )
        .fill(theme.bg_tertiary)
        .stroke(egui::Stroke::NONE)
        .rounding(egui::Rounding::same(6.0)),
    )
    .on_hover_text(tooltip)
}

struct ChannelRowParams {
    row_id: String,
    icon: &'static str,
    label: String,
    active: bool,
    unread: bool,
    metadata: Option<String>,
}

fn channel_row(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    params: ChannelRowParams,
) -> egui::Response {
    let ChannelRowParams {
        row_id,
        icon,
        label,
        active,
        unread,
        metadata,
    } = params;

    let id = ui.make_persistent_id(row_id);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), CHANNEL_ROW_HEIGHT),
        egui::Sense::click(),
    );

    let hovered = response.hovered();
    let hover_t = ctx.animate_bool(id.with("hover"), hovered);

    let fill = if active {
        theme.bg_active
    } else {
        Theme::lerp_color(theme.bg_secondary, theme.bg_hover, hover_t * 0.45)
    };
    ui.painter()
        .rect_filled(rect, egui::Rounding::same(4.0), fill);

    let icon_color = if active {
        theme.text_primary
    } else {
        theme.text_muted
    };
    let text_color = if active || unread {
        theme.text_primary
    } else {
        theme.text_muted
    };

    let text_pos = egui::pos2(rect.left() + 10.0, rect.center().y);
    ui.painter().text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        icon,
        egui::FontId::proportional(15.0),
        icon_color,
    );
    ui.painter().text(
        text_pos + egui::vec2(18.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(15.0),
        text_color,
    );

    if let Some(meta) = metadata.filter(|value| !value.is_empty()) {
        ui.painter().text(
            egui::pos2(rect.right() - 12.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            meta,
            egui::FontId::proportional(12.0),
            theme.text_muted,
        );
    }

    if unread && !active {
        ui.painter().circle_filled(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            3.0,
            theme.text_primary,
        );
    }

    response
}

fn voice_participant_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    participant: &VoiceParticipant,
    is_speaking: bool,
    is_self: bool,
    is_full_muted: bool,
    is_locally_muted: bool,
) -> egui::Response {
    let name = if participant.username.is_empty() {
        "Гость"
    } else {
        participant.username.as_str()
    };

    let row = ui
        .horizontal(|ui| {
            ui.add_space(20.0);
            crate::components::avatar::avatar(ui, theme, name, 12.0, is_speaking, None);
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(name)
                    .size(13.0)
                    .color(theme.text_secondary),
            );
            if is_locally_muted {
                ui.label(
                    egui::RichText::new("mute local")
                        .size(12.0)
                        .color(theme.warning),
                );
            } else if is_full_muted {
                ui.label(
                    egui::RichText::new("Полный мут")
                        .size(12.0)
                        .color(theme.error),
                );
            } else if participant.mic_muted || (is_self && participant.mic_muted) {
                ui.label(
                    egui::RichText::new("Микрофон выключен")
                        .size(12.0)
                        .color(theme.error),
                );
            }
        })
        .response;
    ui.add_space(2.0);
    row
}
