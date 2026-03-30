//! Central chat area: header, messages, and composer.

use std::collections::HashMap;

use eframe::egui;

use crate::net::{AttachmentMeta, Member, Message};
use crate::theme::Theme;

const CHANNEL_HEADER_HEIGHT: f32 = 48.0;
const TOOLBAR_ICON_SIZE: f32 = 24.0;
const COMPOSER_BUTTON_SIZE: f32 = 32.0;
const SEARCH_WIDTH: f32 = 240.0;
const SEARCH_RESULTS_MAX_HEIGHT: f32 = 340.0;
const MESSAGE_INPUT_MIN_HEIGHT: f32 = 34.0;
const MESSAGE_INPUT_MAX_HEIGHT: f32 = 140.0;

#[derive(Debug, Clone)]
pub struct ChatSearchResult {
    pub channel_id: i64,
    pub channel_name: String,
    pub message: Message,
}

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
    pub search_query: &'a mut String,
    pub search_results: &'a [ChatSearchResult],
    pub search_loading: bool,
    pub search_error: Option<&'a str>,
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

pub fn show(ctx: &egui::Context, ui: &mut egui::Ui, params: ChatPanelParams<'_>) {
    let ChatPanelParams {
        theme,
        channel_name,
        channel_description,
        messages,
        search_query,
        search_results,
        search_loading,
        search_error,
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

    let message_rows = estimate_message_rows(new_message, ui.available_width());
    let input_height =
        (MESSAGE_INPUT_MIN_HEIGHT + (message_rows.saturating_sub(1) as f32 * 18.0))
            .clamp(MESSAGE_INPUT_MIN_HEIGHT, MESSAGE_INPUT_MAX_HEIGHT);
    let composer_base_height = crate::bottom_panel::BASE_PANEL_HEIGHT;
    let composer_row_height = (composer_base_height + (input_height - MESSAGE_INPUT_MIN_HEIGHT))
        .max(composer_base_height);
    let typing_height = if typing_users.is_empty() { 0.0 } else { 24.0 };
    let attachment_height = if pending_attachment.is_some() { 42.0 } else { 0.0 };
    let composer_height = composer_row_height + typing_height + attachment_height;
    let mut search_anchor_rect = None;

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
                    if toolbar_icon_button(ui, theme, "M", "Toggle members").clicked() {
                        (*on_action)(ChatPanelAction::ToggleMemberList);
                    }
                    if toolbar_icon_button(ui, theme, "P", "Pinned").clicked() {
                        (*on_action)(ChatPanelAction::Pinned);
                    }
                    if toolbar_icon_button(ui, theme, "N", "Notifications").clicked() {
                        (*on_action)(ChatPanelAction::Notifications);
                    }
                    let search = search_field(ui, theme, search_query);
                    search_anchor_rect = Some(search.rect);
                });
            });
        });

    if !search_query.trim().is_empty() {
        if let Some(anchor_rect) = search_anchor_rect {
            show_search_results_popup(
                ctx,
                theme,
                anchor_rect,
                server_members,
                search_results,
                search_loading,
                search_error,
            );
        }
    }

    egui::TopBottomPanel::bottom("chat_composer")
        .exact_height(composer_height)
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter()
                .rect_filled(rect, egui::Rounding::ZERO, theme.bg_primary);
            ui.painter().line_segment(
                [rect.left_top(), rect.right_top()],
                egui::Stroke::new(1.0, theme.border),
            );

            let composer_rect = egui::Rect::from_min_size(
                egui::pos2(rect.left(), rect.bottom() - composer_row_height),
                egui::vec2(rect.width(), composer_row_height),
            );
            let meta_rect = egui::Rect::from_min_max(rect.min, composer_rect.min);

            if meta_rect.height() > 0.0 {
                ui.allocate_ui_at_rect(meta_rect, |ui| {
                    ui.set_width(meta_rect.width());
                    ui.spacing_mut().item_spacing.y = 6.0;
                    ui.add_space(8.0);

                    if let Some(attachment) = pending_attachment {
                        egui::Frame::none()
                            .fill(theme.bg_secondary)
                            .rounding(egui::Rounding::same(8.0))
                            .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Attached: {}",
                                            attachment.filename
                                        ))
                                        .color(theme.text_secondary),
                                    );
                                    if ui.button("Remove").clicked() {
                                        (*on_action)(ChatPanelAction::ClearAttachment);
                                    }
                                });
                            });
                    }

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
                    }
                });
            }

            ui.allocate_ui_at_rect(composer_rect, |ui| {
                ui.set_width(composer_rect.width());
                egui::Frame::none()
                    .fill(theme.bg_quaternary)
                    .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            button_with_vertical_offset(
                                ui,
                                ((input_height - COMPOSER_BUTTON_SIZE) * 0.5).max(0.0),
                                |ui| {
                                    if composer_button_sized(
                                        ui,
                                        theme,
                                        "+",
                                        "Attach file",
                                        21.0,
                                    )
                                    .clicked()
                                    {
                                        (*on_action)(ChatPanelAction::AttachRequest);
                                    }
                                },
                            );

                            let side_buttons_width = COMPOSER_BUTTON_SIZE * 3.0 + 18.0;
                            let input_w = (ui.available_width() - side_buttons_width).max(120.0);
                            let response = ui.add_sized(
                                [input_w, input_height],
                                egui::TextEdit::multiline(new_message)
                                    .desired_rows(1)
                                    .lock_focus(true)
                                    .hint_text(format!(
                                        "Написать в #{}",
                                        channel_name.trim_start_matches("# ")
                                    )),
                            );

                            button_with_vertical_offset(
                                ui,
                                ((input_height - COMPOSER_BUTTON_SIZE) * 0.5).max(0.0),
                                |ui| {
                                    if composer_button(ui, theme, "GIF", "Insert GIF").clicked() {
                                        (*on_action)(ChatPanelAction::StubGif);
                                    }
                                },
                            );
                            button_with_vertical_offset(
                                ui,
                                ((input_height - COMPOSER_BUTTON_SIZE) * 0.5).max(0.0),
                                |ui| {
                                    if composer_button(ui, theme, ":)", "Emoji picker").clicked() {
                                        (*on_action)(ChatPanelAction::StubEmoji);
                                    }
                                },
                            );
                            button_with_vertical_offset(
                                ui,
                                ((input_height - COMPOSER_BUTTON_SIZE) * 0.5).max(0.0),
                                |ui| {
                                    if composer_button(ui, theme, "Send", "Send message").clicked()
                                    {
                                        (*on_action)(ChatPanelAction::SendMessage);
                                    }
                                },
                            );

                            let enter_send = response.has_focus()
                                && ui.input(|input| {
                                    input.key_pressed(egui::Key::Enter)
                                        && !input.modifiers.shift
                                        && !input.modifiers.ctrl
                                });
                            if enter_send {
                                (*on_action)(ChatPanelAction::SendMessage);
                            }
                        });
                    });
            });
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

fn search_field(ui: &mut egui::Ui, theme: &Theme, search_query: &mut String) -> egui::Response {
    egui::Frame::none()
        .fill(theme.bg_quaternary)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(10.0, 5.0))
        .show(ui, |ui| {
            ui.add_sized(
                [SEARCH_WIDTH, TOOLBAR_ICON_SIZE],
                egui::TextEdit::singleline(search_query)
                    .hint_text("Поиск по серверу")
                    .desired_width(SEARCH_WIDTH),
            )
        })
        .inner
}

fn composer_button(ui: &mut egui::Ui, theme: &Theme, label: &str, tooltip: &str) -> egui::Response {
    composer_button_sized(ui, theme, label, tooltip, 11.0)
}

fn composer_button_sized(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    tooltip: &str,
    font_size: f32,
) -> egui::Response {
    ui.add_sized(
        [COMPOSER_BUTTON_SIZE, COMPOSER_BUTTON_SIZE],
        egui::Button::new(
            egui::RichText::new(label)
                .size(font_size)
                .color(theme.text_secondary),
        )
        .frame(false),
    )
    .on_hover_text(tooltip)
}

fn button_with_vertical_offset(
    ui: &mut egui::Ui,
    top_offset: f32,
    add_button: impl FnOnce(&mut egui::Ui),
) {
    ui.vertical(|ui| {
        if top_offset > 0.0 {
            ui.add_space(top_offset);
        }
        add_button(ui);
    });
}

fn show_search_results_popup(
    ctx: &egui::Context,
    theme: &Theme,
    anchor_rect: egui::Rect,
    server_members: &[Member],
    search_results: &[ChatSearchResult],
    search_loading: bool,
    search_error: Option<&str>,
) {
    let screen_rect = ctx.screen_rect();
    let width = 420.0_f32.min(screen_rect.width() - 24.0).max(300.0);
    let max_x = (screen_rect.right() - width - 12.0).max(screen_rect.left() + 12.0);
    let x = (anchor_rect.right() - width).clamp(screen_rect.left() + 12.0, max_x);
    let y = anchor_rect.bottom() + 6.0;

    egui::Area::new(egui::Id::new("chat_search_results_popup"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(x, y))
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(theme.bg_secondary)
                .rounding(egui::Rounding::same(10.0))
                .stroke(egui::Stroke::new(1.0, theme.border))
                .inner_margin(egui::Margin::symmetric(10.0, 10.0))
                .show(ui, |ui| {
                    ui.set_width(width);

                    if search_loading {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Поиск сообщений...")
                                    .color(theme.text_muted),
                            );
                        });
                        return;
                    }

                    if let Some(error) = search_error {
                        ui.label(egui::RichText::new(error).color(theme.error));
                        return;
                    }

                    if search_results.is_empty() {
                        ui.label(
                            egui::RichText::new("совпадений не найдено")
                                .color(theme.text_muted),
                        );
                        return;
                    }

                    egui::ScrollArea::vertical()
                        .max_height(SEARCH_RESULTS_MAX_HEIGHT)
                        .show(ui, |ui| {
                            for result in search_results {
                                let author_name =
                                    resolve_message_author_name(&result.message, server_members);
                                egui::Frame::none()
                                    .fill(theme.bg_quaternary)
                                    .rounding(egui::Rounding::same(8.0))
                                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                                    .show(ui, |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(
                                                egui::RichText::new(author_name)
                                                    .strong()
                                                    .color(theme.text_primary),
                                            );
                                            ui.label(
                                                egui::RichText::new(format_message_timestamp(
                                                    &result.message.created_at,
                                                ))
                                                .size(11.0)
                                                .color(theme.text_muted),
                                            );
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "#{}",
                                                    result.channel_name
                                                ))
                                                .size(11.0)
                                                .color(theme.text_muted),
                                            );
                                        });
                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new(message_preview_text(
                                                &result.message,
                                            ))
                                            .color(theme.text_secondary),
                                        );
                                    });
                                ui.add_space(8.0);
                            }
                        });
                });
        });
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
    let author_name = resolve_message_author_name(message, server_members);
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(16.0, 6.0))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                crate::components::avatar::avatar(
                    ui,
                    theme,
                    &author_name,
                    18.0,
                    false,
                    avatar_texture,
                );
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(&author_name)
                                .size(15.0)
                                .strong()
                                .color(theme.text_primary),
                        );
                        ui.label(
                            egui::RichText::new(format_message_timestamp(&message.created_at))
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
                                    .map(display_name_for_member)
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

fn resolve_message_author_name(message: &Message, server_members: &[Member]) -> String {
    server_members
        .iter()
        .find(|member| member.user_id == message.author_id)
        .map(display_name_for_member)
        .unwrap_or_else(|| message.author_username.clone())
}

fn display_name_for_member(member: &Member) -> String {
    if member.display_name.trim().is_empty() {
        member.username.clone()
    } else {
        member.display_name.clone()
    }
}

fn format_message_timestamp(created_at: &str) -> String {
    let normalized = created_at.trim();
    if normalized.len() >= 16
        && normalized.as_bytes().get(4) == Some(&b'-')
        && normalized.as_bytes().get(7) == Some(&b'-')
        && matches!(normalized.as_bytes().get(10), Some(b'T') | Some(b' '))
        && normalized.as_bytes().get(13) == Some(&b':')
    {
        let year = &normalized[0..4];
        let month = &normalized[5..7];
        let day = &normalized[8..10];
        let hour = &normalized[11..13];
        let minute = &normalized[14..16];
        return format!("{day}.{month}.{year} {hour}:{minute}");
    }
    normalized.to_string()
}

fn message_preview_text(message: &Message) -> String {
    if !message.content.trim().is_empty() {
        return message.content.clone();
    }
    let attachment_names = message
        .attachments
        .iter()
        .map(|attachment| attachment.filename.as_str())
        .collect::<Vec<_>>();
    if attachment_names.is_empty() {
        "Пустое сообщение".to_string()
    } else {
        attachment_names.join(", ")
    }
}

fn estimate_message_rows(text: &str, available_width: f32) -> usize {
    let width_for_text = (available_width - 220.0).max(140.0);
    let approx_chars_per_row = (width_for_text / 7.2).floor().max(12.0) as usize;
    let rows = text
        .split('\n')
        .map(|line| {
            let len = line.chars().count().max(1);
            ((len.saturating_sub(1)) / approx_chars_per_row) + 1
        })
        .sum::<usize>();
    rows.max(1)
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
