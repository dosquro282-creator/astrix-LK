//! Central chat area: header, messages, and composer.

use std::collections::HashMap;

use eframe::egui;
use egui::scroll_area::ScrollBarVisibility;

use crate::net::{AttachmentMeta, Member, Message};
use crate::theme::Theme;

const CHANNEL_HEADER_HEIGHT: f32 = 48.0;
const HEADER_SIDE_PADDING: f32 = 10.0;
const HEADER_ZONE_GAP: f32 = 10.0;
const TOOLBAR_ICON_SIZE: f32 = 24.0;
const COMPOSER_BUTTON_SIZE: f32 = 32.0;
const SEARCH_WIDTH: f32 = 360.0;
const SEARCH_FIELD_HORIZONTAL_PADDING: f32 = 10.0;
const SEARCH_SCROLLBAR_WIDTH: f32 = 10.0;
const SEARCH_SCROLLBAR_GAP: f32 = 8.0;
const SEARCH_RESULT_ROW_HEIGHT: f32 = 66.0;
const SEARCH_RESULT_GAP: f32 = 8.0;
const SEARCH_RESULTS_MAX_HEIGHT: f32 = 600.0;
const SEARCH_RESULTS_RIGHT_PADDING: f32 = 0.0;
const SEARCH_RESULTS_VERTICAL_PADDING: f32 = 10.0;
const MESSAGE_INPUT_BASE_HEIGHT: f32 = 48.0;
const MESSAGE_INPUT_MAX_HEIGHT: f32 = 156.0;
const COMPOSER_SIDE_MARGIN: f32 = 5.0;

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
    OpenSearchResult { channel_id: i64, message_id: i64 },
    KickMember(i64),
    BanMember(i64),
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
    pub search_popup_open: &'a mut bool,
    pub search_results: &'a [ChatSearchResult],
    pub search_scroll_offset: &'a mut f32,
    pub search_loading: bool,
    pub search_error: Option<&'a str>,
    pub highlighted_message_id: Option<i64>,
    pub highlighted_message_t: Option<f32>,
    pub scroll_to_highlighted: &'a mut bool,
    pub new_message: &'a mut String,
    pub typing_users: &'a [(i64, String)],
    pub pending_attachment: Option<&'a AttachmentMeta>,
    pub current_user_id: Option<i64>,
    pub can_moderate_members: bool,
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
        search_popup_open,
        search_results,
        search_scroll_offset,
        search_loading,
        search_error,
        highlighted_message_id,
        highlighted_message_t,
        scroll_to_highlighted,
        new_message,
        typing_users,
        pending_attachment,
        current_user_id,
        can_moderate_members,
        server_members,
        media_textures,
        media_bytes,
        avatar_textures,
        on_action,
        messages_load_error,
        messages_loading,
    } = params;

    let chat_rect = ui.max_rect();
    ui.painter()
        .rect_filled(chat_rect, egui::Rounding::ZERO, theme.bg_primary);

    let message_rows = estimate_message_rows(new_message, chat_rect.width());
    let input_height = (MESSAGE_INPUT_BASE_HEIGHT + (message_rows.saturating_sub(1) as f32 * 18.0))
        .clamp(MESSAGE_INPUT_BASE_HEIGHT, MESSAGE_INPUT_MAX_HEIGHT);
    let composer_base_height = crate::bottom_panel::BASE_PANEL_HEIGHT;
    let composer_row_height = (composer_base_height + (input_height - MESSAGE_INPUT_BASE_HEIGHT))
        .max(composer_base_height);
    let typing_height = if typing_users.is_empty() { 0.0 } else { 24.0 };
    let attachment_height = if pending_attachment.is_some() {
        42.0
    } else {
        0.0
    };
    let composer_height = composer_row_height + typing_height + attachment_height;
    let mut search_anchor_rect = None;
    let mut search_popup_rect = None;

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

            let header_rect = rect.shrink2(egui::vec2(HEADER_SIDE_PADDING, 0.0));
            let button_spacing = ui.spacing().item_spacing.x;
            let button_zone_width = (TOOLBAR_ICON_SIZE * 3.0) + (button_spacing * 2.0);
            let max_search_outer_width = SEARCH_WIDTH + (SEARCH_FIELD_HORIZONTAL_PADDING * 2.0);
            let available_search_width =
                (header_rect.width() - ((button_zone_width + HEADER_ZONE_GAP) * 2.0)).max(1.0);
            let search_outer_width = available_search_width.min(max_search_outer_width);
            let side_zone_width = ((header_rect.width() - search_outer_width) * 0.5).max(0.0);
            let left_rect = egui::Rect::from_min_size(
                header_rect.min,
                egui::vec2(side_zone_width, header_rect.height()),
            );
            let search_rect = egui::Rect::from_min_size(
                egui::pos2(left_rect.right(), header_rect.top()),
                egui::vec2(search_outer_width, header_rect.height()),
            );
            let right_rect = egui::Rect::from_min_max(
                egui::pos2(search_rect.right(), header_rect.top()),
                header_rect.max,
            );
            let search_field_rect = egui::Rect::from_center_size(
                search_rect.center(),
                egui::vec2(search_outer_width, TOOLBAR_ICON_SIZE + 10.0),
            );

            ui.allocate_ui_at_rect(left_rect, |ui| {
                ui.set_clip_rect(left_rect);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
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
                    if let Some(description) = channel_description.filter(|value| !value.is_empty())
                    {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(description)
                                .size(12.0)
                                .color(theme.text_muted),
                        );
                    }
                });
            });

            ui.allocate_ui_at_rect(search_field_rect, |ui| {
                let search = search_field(ui, theme, search_query, search_outer_width);
                search_anchor_rect = Some(search.rect);
                if search.clear_clicked {
                    search_query.clear();
                    *search_popup_open = false;
                } else if search.text_response.changed()
                    || search.text_response.clicked()
                    || search.text_response.gained_focus()
                    || search.text_response.has_focus()
                {
                    *search_popup_open = true;
                }
            });

            ui.allocate_ui_at_rect(right_rect, |ui| {
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
                });
            });
        });

    if !search_query.trim().is_empty() && *search_popup_open {
        if let Some(anchor_rect) = search_anchor_rect {
            search_popup_rect = show_search_results_popup(
                ctx,
                theme,
                chat_rect,
                anchor_rect,
                server_members,
                search_results,
                search_scroll_offset,
                search_loading,
                search_error,
                avatar_textures,
                on_action,
            );
        }
    }

    if *search_popup_open && !search_query.trim().is_empty() {
        if let Some(pointer_pos) = ctx.input(|input| {
            if input.pointer.any_pressed() {
                input.pointer.interact_pos()
            } else {
                None
            }
        }) {
            let inside_search = search_anchor_rect
                .map(|rect| rect.contains(pointer_pos))
                .unwrap_or(false);
            let inside_popup = search_popup_rect
                .map(|rect| rect.contains(pointer_pos))
                .unwrap_or(false);
            if !inside_search && !inside_popup {
                *search_popup_open = false;
            }
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

            let content_rect = rect.shrink2(egui::vec2(COMPOSER_SIDE_MARGIN, 0.0));
            let composer_rect = egui::Rect::from_min_size(
                egui::pos2(
                    content_rect.left(),
                    content_rect.bottom() - composer_row_height,
                ),
                egui::vec2(content_rect.width(), composer_row_height),
            );
            let meta_rect = egui::Rect::from_min_max(content_rect.min, composer_rect.min);

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
                    .inner_margin(egui::Margin {
                        left: 12.0,
                        right: 12.0,
                        top: 10.0,
                        bottom: 0.0,
                    })
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            let button_offset =
                                ((input_height - COMPOSER_BUTTON_SIZE) * 0.5).max(0.0);
                            let composer_gap = ui.spacing().item_spacing.x;
                            let right_reserved_width =
                                COMPOSER_BUTTON_SIZE * 3.0 + composer_gap * 3.0;
                            button_with_vertical_offset(ui, button_offset, |ui| {
                                if composer_button_sized(ui, theme, "+", "Attach file", 21.0)
                                    .clicked()
                                {
                                    (*on_action)(ChatPanelAction::AttachRequest);
                                }
                            });
                            let left_extra_space =
                                (right_reserved_width - COMPOSER_BUTTON_SIZE - composer_gap)
                                    .max(0.0);
                            if left_extra_space > 0.0 {
                                ui.add_space(left_extra_space);
                            }
                            let input_w = (ui.available_width() - right_reserved_width).max(120.0);
                            let _composer_placeholder = format!(
                                "РќР°РїРёСЃР°С‚СЊ РІ #{}",
                                channel_name.trim_start_matches("# ")
                            );
                            let response = ui.add_sized(
                                [input_w, input_height],
                                egui::TextEdit::multiline(new_message)
                                    .desired_rows(1)
                                    .lock_focus(true)
                                    .margin(egui::Margin::symmetric(4.0, 0.0))
                                    .horizontal_align(egui::Align::Min)
                                    .vertical_align(egui::Align::Center),
                                /*
                                    "Написать в #{}",
                                */
                            );
                            let composer_placeholder =
                                format!("Написать в #{}", channel_name.trim_start_matches("# "));
                            if new_message.is_empty() && !response.has_focus() {
                                let placeholder_rect = response.rect.shrink2(egui::vec2(8.0, 0.0));
                                ui.painter().with_clip_rect(placeholder_rect).text(
                                    egui::pos2(
                                        placeholder_rect.left(),
                                        placeholder_rect.center().y,
                                    ),
                                    egui::Align2::LEFT_CENTER,
                                    composer_placeholder,
                                    egui::FontId::proportional(15.0),
                                    theme.text_muted,
                                );
                            }

                            button_with_vertical_offset(ui, button_offset, |ui| {
                                if composer_button(ui, theme, "GIF", "Insert GIF").clicked() {
                                    (*on_action)(ChatPanelAction::StubGif);
                                }
                            });
                            button_with_vertical_offset(ui, button_offset, |ui| {
                                if composer_button(ui, theme, ":)", "Emoji picker").clicked() {
                                    (*on_action)(ChatPanelAction::StubEmoji);
                                }
                            });
                            button_with_vertical_offset(ui, button_offset, |ui| {
                                if composer_button(ui, theme, "Send", "Send message").clicked() {
                                    (*on_action)(ChatPanelAction::SendMessage);
                                }
                            });

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
        .stick_to_bottom(!*scroll_to_highlighted)
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
                let is_highlighted = highlighted_message_id == Some(message.id);
                if message_row(
                    ui,
                    theme,
                    message,
                    current_user_id,
                    server_members,
                    can_moderate_members,
                    media_textures,
                    media_bytes,
                    avatar_textures.get(&message.author_id),
                    on_action,
                    if is_highlighted {
                        highlighted_message_t
                    } else {
                        None
                    },
                    is_highlighted && *scroll_to_highlighted,
                ) {
                    *scroll_to_highlighted = false;
                }
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

struct SearchFieldResponse {
    rect: egui::Rect,
    text_response: egui::Response,
    clear_clicked: bool,
}

fn search_field(
    ui: &mut egui::Ui,
    theme: &Theme,
    search_query: &mut String,
    outer_width: f32,
) -> SearchFieldResponse {
    let inner_width = (outer_width - (SEARCH_FIELD_HORIZONTAL_PADDING * 2.0)).max(1.0);
    let frame = egui::Frame::none()
        .fill(theme.bg_quaternary)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(
            SEARCH_FIELD_HORIZONTAL_PADDING,
            5.0,
        ))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let clear_width = if search_query.is_empty() {
                    0.0
                } else {
                    TOOLBAR_ICON_SIZE
                };
                let spacing = if search_query.is_empty() { 0.0 } else { 4.0 };
                let text_width = (inner_width - clear_width - spacing).max(1.0);
                let text_response = ui.add_sized(
                    [text_width, TOOLBAR_ICON_SIZE],
                    egui::TextEdit::singleline(search_query)
                        .hint_text("Поиск по серверу")
                        .horizontal_align(egui::Align::Min)
                        .vertical_align(egui::Align::Center)
                        .desired_width(text_width),
                );
                let clear_clicked = if search_query.is_empty() {
                    false
                } else {
                    if spacing > 0.0 {
                        ui.add_space(spacing);
                    }
                    ui.add_sized(
                        [TOOLBAR_ICON_SIZE, TOOLBAR_ICON_SIZE],
                        egui::Button::new(
                            egui::RichText::new("×")
                                .size(15.0)
                                .color(theme.text_secondary),
                        )
                        .frame(false),
                    )
                    .on_hover_text("Очистить поиск")
                    .clicked()
                };

                SearchFieldResponse {
                    rect: ui.min_rect(),
                    text_response,
                    clear_clicked,
                }
            })
            .inner
        });
    let mut response = frame.inner;
    response.rect = frame.response.rect;
    response
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
    chat_rect: egui::Rect,
    anchor_rect: egui::Rect,
    server_members: &[Member],
    search_results: &[ChatSearchResult],
    search_scroll_offset: &mut f32,
    search_loading: bool,
    search_error: Option<&str>,
    avatar_textures: &HashMap<i64, egui::TextureHandle>,
    on_action: &mut dyn FnMut(ChatPanelAction),
) -> Option<egui::Rect> {
    let width = chat_rect.width().max(1.0);
    let y = anchor_rect.bottom() + 6.0;
    let mut popup_rect = None;

    egui::Area::new(egui::Id::new("chat_search_results_popup"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(chat_rect.left(), y))
        .show(ctx, |ui| {
            let frame = egui::Frame::none()
                .fill(theme.bg_secondary)
                .rounding(egui::Rounding::same(10.0))
                .stroke(egui::Stroke::new(1.0, theme.border))
                .inner_margin(egui::Margin {
                    left: 0.0,
                    right: 0.0,
                    top: 0.0,
                    bottom: 0.0,
                })
                .show(ui, |ui| {
                    ui.set_width(width);

                    if search_loading {
                        ui.add_space(SEARCH_RESULTS_VERTICAL_PADDING);
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Поиск сообщений...").color(theme.text_muted),
                            );
                        });
                        ui.add_space(SEARCH_RESULTS_VERTICAL_PADDING);
                        return;
                    }

                    if let Some(error) = search_error {
                        ui.add_space(SEARCH_RESULTS_VERTICAL_PADDING);
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.label(egui::RichText::new(error).color(theme.error));
                        });
                        ui.add_space(SEARCH_RESULTS_VERTICAL_PADDING);
                        return;
                    }

                    if search_results.is_empty() {
                        ui.add_space(SEARCH_RESULTS_VERTICAL_PADDING);
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new("совпадений не найдено")
                                    .color(theme.text_muted),
                            );
                        });
                        ui.add_space(SEARCH_RESULTS_VERTICAL_PADDING);
                        return;
                    }

                    let content_height = (search_results.len() as f32 * SEARCH_RESULT_ROW_HEIGHT)
                        + (search_results.len().saturating_sub(1) as f32 * SEARCH_RESULT_GAP);
                    let viewport_height = content_height.min(SEARCH_RESULTS_MAX_HEIGHT);
                    let popup_height = viewport_height + (SEARCH_RESULTS_VERTICAL_PADDING * 2.0);
                    let results_width = (width
                        - SEARCH_SCROLLBAR_WIDTH
                        - SEARCH_SCROLLBAR_GAP
                        - SEARCH_RESULTS_RIGHT_PADDING)
                        .max(1.0);
                    let mut scroll_metrics = None;

                    let (popup_rect, _) = ui
                        .allocate_exact_size(egui::vec2(width, popup_height), egui::Sense::hover());
                    let results_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            popup_rect.left(),
                            popup_rect.top() + SEARCH_RESULTS_VERTICAL_PADDING,
                        ),
                        egui::vec2(results_width, viewport_height),
                    );
                    let scroll_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            results_rect.right() + SEARCH_SCROLLBAR_GAP,
                            results_rect.top(),
                        ),
                        egui::vec2(SEARCH_SCROLLBAR_WIDTH, viewport_height),
                    );

                    ui.allocate_ui_at_rect(results_rect, |ui| {
                        let output = egui::ScrollArea::vertical()
                            .id_source("chat_search_results_popup_scroll")
                            .vertical_scroll_offset(*search_scroll_offset)
                            .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(results_rect.width());
                                for (index, result) in search_results.iter().enumerate() {
                                    search_result_row(
                                        ui,
                                        theme,
                                        result,
                                        server_members,
                                        avatar_textures,
                                        results_rect.width(),
                                        on_action,
                                    );
                                    if index + 1 != search_results.len() {
                                        ui.add_space(SEARCH_RESULT_GAP);
                                    }
                                }
                            });
                        *search_scroll_offset = output.state.offset.y;
                        scroll_metrics = Some((
                            output.content_size.y,
                            output.inner_rect.height(),
                            output.state.offset.y,
                        ));
                    });

                    if let Some((content_size_y, viewport_size_y, offset_y)) = scroll_metrics {
                        paint_left_scrollbar(
                            ui,
                            theme,
                            scroll_rect,
                            content_size_y,
                            viewport_size_y,
                            offset_y,
                            search_scroll_offset,
                            ui.id().with("chat_search_results_left_scroll"),
                        );
                    }
                });
            popup_rect = Some(frame.response.rect);
        });
    popup_rect
}

fn search_result_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    result: &ChatSearchResult,
    server_members: &[Member],
    avatar_textures: &HashMap<i64, egui::TextureHandle>,
    width: f32,
    on_action: &mut dyn FnMut(ChatPanelAction),
) {
    let response = ui.add_sized(
        [width, SEARCH_RESULT_ROW_HEIGHT],
        egui::Button::new("")
            .frame(false)
            .sense(egui::Sense::click()),
    );
    let rect = response.rect;
    let fill = if response.hovered() {
        theme.bg_hover
    } else {
        theme.bg_quaternary
    };
    let stroke = if response.hovered() {
        egui::Stroke::new(1.0, theme.border_strong)
    } else {
        egui::Stroke::new(1.0, theme.border)
    };
    ui.painter()
        .rect(rect, egui::Rounding::same(8.0), fill, stroke);

    let author_name = resolve_message_author_name(&result.message, server_members);
    let avatar_texture = avatar_textures.get(&result.message.author_id);
    let title = format!(
        "{}  {}  #{}",
        author_name,
        format_message_timestamp(&result.message.created_at),
        result.channel_name
    );
    let preview = message_preview_text(&result.message);
    let content_rect = rect.shrink2(egui::vec2(10.0, 8.0));
    let avatar_radius = 18.0;
    let avatar_rect = egui::Rect::from_min_size(
        egui::pos2(content_rect.left(), content_rect.top()),
        egui::vec2(avatar_radius * 2.0, avatar_radius * 2.0),
    );
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(avatar_rect.right() + 10.0, content_rect.top()),
        content_rect.max,
    );

    ui.allocate_ui_at_rect(avatar_rect, |ui| {
        crate::components::avatar::avatar(
            ui,
            theme,
            &author_name,
            avatar_radius,
            false,
            avatar_texture,
        );
    });
    paint_search_result_text(
        ui,
        text_rect,
        &title,
        egui::FontId::proportional(14.0),
        theme.text_primary,
    );
    paint_search_result_text(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(text_rect.left(), text_rect.top() + 22.0),
            text_rect.max,
        ),
        &preview,
        egui::FontId::proportional(14.0),
        theme.text_secondary,
    );

    if response.clicked() {
        on_action(ChatPanelAction::OpenSearchResult {
            channel_id: result.channel_id,
            message_id: result.message.id,
        });
    }
}

fn paint_search_result_text(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    text: &str,
    font_id: egui::FontId,
    color: egui::Color32,
) {
    let galley = ui
        .painter()
        .layout(text.to_owned(), font_id, color, f32::INFINITY);
    ui.painter()
        .with_clip_rect(rect)
        .galley(rect.left_top(), galley, color);
}

fn paint_left_scrollbar(
    ui: &mut egui::Ui,
    theme: &Theme,
    rect: egui::Rect,
    content_height: f32,
    viewport_height: f32,
    offset_y: f32,
    search_scroll_offset: &mut f32,
    id: egui::Id,
) {
    ui.painter()
        .rect_filled(rect, egui::Rounding::same(4.0), theme.bg_elevated);

    if content_height <= viewport_height + 0.5 {
        return;
    }

    let max_offset = (content_height - viewport_height).max(1.0);
    let handle_height =
        ((viewport_height / content_height) * rect.height()).clamp(28.0, rect.height());
    let track_height = (rect.height() - handle_height).max(1.0);
    let handle_top = rect.top() + (offset_y / max_offset) * track_height;
    let handle_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left(), handle_top),
        egui::vec2(rect.width(), handle_height),
    );

    let response = ui.interact(rect, id, egui::Sense::click_and_drag());
    if (response.clicked() || response.dragged()) && track_height > 0.0 {
        if let Some(pointer) = response.interact_pointer_pos() {
            let t =
                ((pointer.y - rect.top() - (handle_height * 0.5)) / track_height).clamp(0.0, 1.0);
            *search_scroll_offset = t * max_offset;
            ui.ctx().request_repaint();
        }
    }

    let handle_fill = if response.dragged() || response.hovered() {
        theme.accent
    } else {
        theme.bg_active
    };
    ui.painter()
        .rect_filled(handle_rect, egui::Rounding::same(4.0), handle_fill);
}

fn message_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    message: &Message,
    current_user_id: Option<i64>,
    server_members: &[Member],
    can_moderate_members: bool,
    media_textures: &HashMap<i64, egui::TextureHandle>,
    media_bytes: &HashMap<i64, (Vec<u8>, String)>,
    avatar_texture: Option<&egui::TextureHandle>,
    on_action: &mut dyn FnMut(ChatPanelAction),
    highlight_t: Option<f32>,
    scroll_to_highlighted: bool,
) -> bool {
    let author_name = resolve_message_author_name(message, server_members);
    let row_fill = highlight_t.map_or(egui::Color32::TRANSPARENT, |t| {
        Theme::lerp_color(
            theme.bg_hover,
            theme.accent,
            (0.18 + t * 0.28).clamp(0.0, 1.0),
        )
    });

    let response = egui::Frame::none()
        .fill(row_fill)
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
        })
        .response;

    if can_moderate_members && Some(message.author_id) != current_user_id {
        response.context_menu(|ui| {
            if ui
                .add(
                    egui::Button::new("Выгнать с сервера")
                        .fill(theme.error)
                        .stroke(egui::Stroke::NONE),
                )
                .clicked()
            {
                (*on_action)(ChatPanelAction::KickMember(message.author_id));
                ui.close_menu();
            }
            if ui
                .add(
                    egui::Button::new("Забанить на сервере")
                        .fill(theme.error)
                        .stroke(egui::Stroke::NONE),
                )
                .clicked()
            {
                (*on_action)(ChatPanelAction::BanMember(message.author_id));
                ui.close_menu();
            }
        });
    }

    if scroll_to_highlighted {
        response.scroll_to_me(Some(egui::Align::Center));
        ui.ctx().request_repaint();
    }
    scroll_to_highlighted
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
