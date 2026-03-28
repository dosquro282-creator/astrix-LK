//! Central chat area: header, messages, and composer.

use std::collections::HashMap;

use eframe::egui;

use crate::net::{AttachmentMeta, Member, Message};
use crate::theme::Theme;

const CHANNEL_HEADER_HEIGHT: f32 = 48.0;
const COMPOSER_HEIGHT: f32 = 88.0;
const TOOLBAR_ICON_SIZE: f32 = 24.0;
const COMPOSER_BUTTON_SIZE: f32 = 32.0;

#[derive(Debug, Clone)]
pub enum ChatPanelAction {
    SendMessage,
    AttachRequest,
    ClearAttachment,
    Threads,
    Notifications,
    Pinned,
    ToggleMemberList,
    Search,
    Inbox,
    Help,
    StubGif,
    StubEmoji,
    StubStickers,
    RetryMessages,
}

pub struct ChatPanelParams<'a> {
    pub theme: &'a Theme,
    pub channel_name: &'a str,
    pub channel_description: Option<&'a str>,
    pub messages: &'a [Message],
    pub new_message: &'a mut String,
    pub typing_users: &'a [(i64, String)],
    pub pending_attachment: Option<&'a AttachmentMeta>,
    pub current_user_id: Option<i64>,
    pub server_members: &'a [Member],
    pub media_textures: &'a HashMap<i64, egui::TextureHandle>,
    pub media_bytes: &'a HashMap<i64, (Vec<u8>, String)>,
    pub avatar_textures: &'a HashMap<i64, egui::TextureHandle>,
    pub on_action: &'a mut dyn FnMut(ChatPanelAction),
    pub messages_load_error: Option<String>,
    pub messages_loading: bool,
}

pub fn show(_ctx: &egui::Context, ui: &mut egui::Ui, params: ChatPanelParams<'_>) {
    let ChatPanelParams {
        theme,
        channel_name,
        channel_description,
        messages,
        new_message,
        typing_users,
        pending_attachment,
        current_user_id,
        server_members,
        media_textures,
        media_bytes,
        avatar_textures,
        on_action,
        messages_load_error,
        messages_loading,
    } = params;

    ui.painter()
        .rect_filled(ui.max_rect(), egui::Rounding::ZERO, theme.bg_primary);

    egui::TopBottomPanel::top("chat_header")
        .exact_height(CHANNEL_HEADER_HEIGHT)
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, theme.bg_primary);
            ui.painter().line_segment(
                [rect.left_bottom(), rect.right_bottom()],
                egui::Stroke::new(1.0, theme.border),
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("#")
                        .size(18.0)
                        .strong()
                        .color(theme.text_muted),
                );
                ui.label(
                    egui::RichText::new(channel_name.trim_start_matches("# "))
                        .size(16.0)
                        .strong()
                        .color(theme.text_primary),
                );
                if let Some(description) = channel_description.filter(|value| !value.is_empty()) {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(description)
                            .size(12.0)
                            .color(theme.text_muted),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if toolbar_icon_button(ui, theme, "?", "Help").clicked() {
                        (*on_action)(ChatPanelAction::Help);
                    }
                    if toolbar_icon_button(ui, theme, "I", "Inbox").clicked() {
                        (*on_action)(ChatPanelAction::Inbox);
                    }
                    let search = search_button(ui, theme);
                    if search.clicked() {
                        (*on_action)(ChatPanelAction::Search);
                    }
                    if toolbar_icon_button(ui, theme, "M", "Toggle members").clicked() {
                        (*on_action)(ChatPanelAction::ToggleMemberList);
                    }
                    if toolbar_icon_button(ui, theme, "P", "Pinned").clicked() {
                        (*on_action)(ChatPanelAction::Pinned);
                    }
                    if toolbar_icon_button(ui, theme, "N", "Notifications").clicked() {
                        (*on_action)(ChatPanelAction::Notifications);
                    }
                    if toolbar_icon_button(ui, theme, "T", "Threads").clicked() {
                        (*on_action)(ChatPanelAction::Threads);
                    }
                });
            });
        });

    egui::TopBottomPanel::bottom("chat_composer")
        .exact_height(COMPOSER_HEIGHT)
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, theme.bg_primary);
            ui.painter().line_segment(
                [rect.left_top(), rect.right_top()],
                egui::Stroke::new(1.0, theme.border),
            );

            ui.add_space(6.0);

            if !typing_users.is_empty() {
                let text = if typing_users.len() == 1 {
                    format!("{} is typing...", typing_users[0].1)
                } else {
                    format!(
                        "{} and {} more are typing...",
                        typing_users[0].1,
                        typing_users.len() - 1
                    )
                };
                ui.label(
                    egui::RichText::new(text)
                        .size(11.0)
                        .italics()
                        .color(theme.text_muted),
                );
                ui.add_space(6.0);
            }

            if let Some(attachment) = pending_attachment {
                egui::Frame::none()
                    .fill(theme.bg_secondary)
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Attached: {}", attachment.filename))
                                    .color(theme.text_secondary),
                            );
                            if ui.button("Remove").clicked() {
                                (*on_action)(ChatPanelAction::ClearAttachment);
                            }
                        });
                    });
                ui.add_space(8.0);
            }

            egui::Frame::none()
                .fill(theme.bg_quaternary)
                .rounding(egui::Rounding::same(8.0))
                .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if composer_button(ui, theme, "+", "Attach file").clicked() {
                            (*on_action)(ChatPanelAction::AttachRequest);
                        }

                        let input_w =
                            (ui.available_width() - COMPOSER_BUTTON_SIZE * 5.0 - 18.0).max(120.0);
                        let response = ui.add_sized(
                            [input_w, 34.0],
                            egui::TextEdit::singleline(new_message)
                                .hint_text(format!("Message {channel_name}")),
                        );

                        if composer_button(ui, theme, "GIF", "Insert GIF").clicked() {
                            (*on_action)(ChatPanelAction::StubGif);
                        }
                        if composer_button(ui, theme, ":)", "Emoji picker").clicked() {
                            (*on_action)(ChatPanelAction::StubEmoji);
                        }
                        if composer_button(ui, theme, "[]", "Stickers").clicked() {
                            (*on_action)(ChatPanelAction::StubStickers);
                        }
                        if composer_button(ui, theme, "Send", "Send message").clicked() {
                            (*on_action)(ChatPanelAction::SendMessage);
                        }

                        let enter_send = response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter));
                        if enter_send {
                            (*on_action)(ChatPanelAction::SendMessage);
                        }
                    });
                });

            ui.add_space(10.0);
        });

    egui::ScrollArea::vertical()
        .id_source("chat_messages_scroll")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(10.0);

            if messages_loading {
                ui.vertical_centered(|ui| {
                    ui.add_space(48.0);
                    ui.spinner();
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Loading messages...").color(theme.text_muted));
                });
                return;
            }

            if let Some(error) = messages_load_error {
                ui.vertical_centered(|ui| {
                    ui.add_space(48.0);
                    ui.label(egui::RichText::new(error).color(theme.error));
                    ui.add_space(8.0);
                    if ui.button("Retry").clicked() {
                        (*on_action)(ChatPanelAction::RetryMessages);
                    }
                });
                return;
            }

            if messages.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.label(
                        egui::RichText::new(format!("Welcome to {channel_name}"))
                            .size(20.0)
                            .strong()
                            .color(theme.text_primary),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("This is the beginning of the channel.")
                            .color(theme.text_muted),
                    );
                });
                return;
            }

            for message in messages {
                message_row(
                    ui,
                    theme,
                    message,
                    current_user_id,
                    server_members,
                    media_textures,
                    media_bytes,
                    avatar_textures.get(&message.author_id),
                );
            }
        });
}

fn toolbar_icon_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: &str,
    tooltip: &str,
) -> egui::Response {
    ui.add_sized(
        [TOOLBAR_ICON_SIZE, TOOLBAR_ICON_SIZE],
        egui::Button::new(
            egui::RichText::new(icon)
                .size(12.0)
                .color(theme.text_secondary),
        )
        .frame(false),
    )
    .on_hover_text(tooltip)
}

fn search_button(ui: &mut egui::Ui, theme: &Theme) -> egui::Response {
    egui::Frame::none()
        .fill(theme.bg_quaternary)
        .rounding(egui::Rounding::same(4.0))
        .inner_margin(egui::Margin::symmetric(8.0, 4.0))
        .show(ui, |ui| {
            ui.add_sized(
                [138.0, TOOLBAR_ICON_SIZE],
                egui::Button::new(
                    egui::RichText::new("Search")
                        .size(12.0)
                        .color(theme.text_muted),
                )
                .frame(false),
            )
            .on_hover_text("Search messages")
        })
        .inner
}

fn composer_button(ui: &mut egui::Ui, theme: &Theme, label: &str, tooltip: &str) -> egui::Response {
    ui.add_sized(
        [COMPOSER_BUTTON_SIZE, COMPOSER_BUTTON_SIZE],
        egui::Button::new(
            egui::RichText::new(label)
                .size(11.0)
                .color(theme.text_secondary),
        )
        .frame(false),
    )
    .on_hover_text(tooltip)
}

fn message_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    message: &Message,
    current_user_id: Option<i64>,
    server_members: &[Member],
    media_textures: &HashMap<i64, egui::TextureHandle>,
    media_bytes: &HashMap<i64, (Vec<u8>, String)>,
    avatar_texture: Option<&egui::TextureHandle>,
) {
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(16.0, 6.0))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                crate::components::avatar::avatar(
                    ui,
                    theme,
                    &message.author_username,
                    18.0,
                    false,
                    avatar_texture,
                );
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(&message.author_username)
                                .size(15.0)
                                .strong()
                                .color(theme.text_primary),
                        );
                        ui.label(
                            egui::RichText::new(&message.created_at)
                                .size(11.0)
                                .color(theme.text_muted),
                        );
                        if Some(message.author_id) == current_user_id {
                            let seen = !message.seen_by.is_empty();
                            let label = ui.label(
                                egui::RichText::new(if seen { "Seen" } else { "Sent" })
                                    .size(11.0)
                                    .color(if seen {
                                        theme.success
                                    } else {
                                        theme.text_muted
                                    }),
                            );
                            if seen {
                                let seen_names = server_members
                                    .iter()
                                    .filter(|member| message.seen_by.contains(&member.user_id))
                                    .map(|member| {
                                        if member.display_name.is_empty() {
                                            member.username.clone()
                                        } else {
                                            member.display_name.clone()
                                        }
                                    })
                                    .collect::<Vec<_>>();
                                if !seen_names.is_empty() {
                                    label.on_hover_text(format!(
                                        "Seen by {}",
                                        seen_names.join(", ")
                                    ));
                                }
                            }
                        }
                    });

                    if !message.content.is_empty()
                        && !message
                            .attachments
                            .iter()
                            .any(|attachment| attachment.filename == message.content)
                    {
                        ui.label(
                            egui::RichText::new(&message.content)
                                .size(15.0)
                                .color(theme.text_secondary),
                        );
                    }

                    for attachment in &message.attachments {
                        ui.add_space(6.0);
                        paint_attachment(ui, theme, attachment, media_textures, media_bytes);
                    }
                });
            });
        });
}

fn paint_attachment(
    ui: &mut egui::Ui,
    theme: &Theme,
    attachment: &AttachmentMeta,
    media_textures: &HashMap<i64, egui::TextureHandle>,
    media_bytes: &HashMap<i64, (Vec<u8>, String)>,
) {
    let media_id = attachment.media_id;
    let is_image = attachment.mime_type.starts_with("image/");
    let is_video = attachment.mime_type.starts_with("video/");

    egui::Frame::none()
        .fill(theme.bg_quaternary)
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            if is_image {
                if let Some(texture) = media_textures.get(&media_id) {
                    let original = texture.size_vec2();
                    let scale = (400.0 / original.x).min(220.0 / original.y).min(1.0);
                    let display = egui::vec2(original.x * scale, original.y * scale);
                    ui.add(egui::Image::new(texture).fit_to_exact_size(display));
                }
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&attachment.filename)
                            .size(12.0)
                            .color(theme.text_muted),
                    );
                    if ui.button("Download").clicked() {
                        save_media_to_disk(media_id, &attachment.filename, media_bytes);
                    }
                });
            } else if is_video {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Video: {}", attachment.filename))
                            .color(theme.text_primary),
                    );
                    if ui.button("Download").clicked() {
                        save_media_to_disk(media_id, &attachment.filename, media_bytes);
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&attachment.filename).color(theme.text_primary));
                    ui.label(
                        egui::RichText::new(format!("({})", fmt_size(attachment.size_bytes)))
                            .size(11.0)
                            .color(theme.text_muted),
                    );
                    if ui.button("Download").clicked() {
                        save_media_to_disk(media_id, &attachment.filename, media_bytes);
                    }
                });
            }
        });
}

fn fmt_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn save_media_to_disk(
    media_id: i64,
    filename: &str,
    media_bytes: &HashMap<i64, (Vec<u8>, String)>,
) {
    if let Some((bytes, _)) = media_bytes.get(&media_id) {
        if let Some(save_path) = rfd::FileDialog::new().set_file_name(filename).save_file() {
            let _ = std::fs::write(save_path, bytes);
        }
    }
}
