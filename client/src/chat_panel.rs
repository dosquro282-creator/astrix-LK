//! Центральная область UI: заголовок канала, кнопки, скроллируемый чат, поле ввода.
//! Discord-like: кнопки Threads, Notifications, Pinned, Member list, Search, Inbox, Help;
//! привязка к отправке сообщений и к данным из state/net.

use std::collections::HashMap;

use eframe::egui;

use crate::net::{AttachmentMeta, Member, Message};
use crate::theme::Theme;

/// Высота заголовка канала (одна строка с кнопками).
const CHANNEL_HEADER_HEIGHT: f32 = 48.0;
/// Высота одной строки поля ввода.
const INPUT_LINE_HEIGHT: f32 = 36.0;
/// Размер иконок кнопок в заголовке и у поля ввода.
const ICON_BUTTON_SIZE: f32 = 32.0;

/// Действие пользователя в панели чата (обрабатывается в ui.rs / app).
#[derive(Debug, Clone)]
pub enum ChatPanelAction {
    /// Отправить сообщение (текст; вложения передаются отдельно через state).
    SendMessage,
    /// Запрос на прикрепление файла — родитель откроет диалог и загрузит медиа.
    AttachRequest,
    /// Убрать прикреплённый файл из поля ввода.
    ClearAttachment,
    /// Кнопки заголовка (заглушки или реальная логика в родителе).
    Threads,
    Notifications,
    Pinned,
    ToggleMemberList,
    Search,
    Inbox,
    Help,
    /// Заглушки: GIF, Emoji, Stickers (логировать в консоль).
    StubGif,
    StubEmoji,
    StubStickers,
    RetryMessages,
}

/// Параметры для отрисовки панели чата.
pub struct ChatPanelParams<'a> {
    pub theme: &'a Theme,
    /// Имя канала для заголовка (например "# general" или "🔊 General").
    pub channel_name: &'a str,
    /// Описание канала (опционально; в Discord под названием).
    pub channel_description: Option<&'a str>,
    /// Сообщения в текущем канале.
    pub messages: &'a [Message],
    /// Текст в поле ввода (мутабельно).
    pub new_message: &'a mut String,
    /// Печатают в канале: (user_id, display_name).
    pub typing_users: &'a [(i64, String)],
    /// Текущее вложение для отправки (если есть).
    pub pending_attachment: Option<&'a AttachmentMeta>,
    /// ID текущего пользователя (для read receipts).
    pub current_user_id: Option<i64>,
    /// Участники сервера (для отображения "видели").
    pub server_members: &'a [Member],
    /// Текстуры медиа по media_id (для превью в сообщениях).
    pub media_textures: &'a HashMap<i64, egui::TextureHandle>,
    /// Байты медиа по media_id (для скачивания).
    pub media_bytes: &'a HashMap<i64, (Vec<u8>, String)>,
    /// Колбэк действий.
    pub on_action: &'a mut dyn FnMut(ChatPanelAction),
    /// Состояние загрузки сообщений: None = загружено, Some(Err) = ошибка.
    pub messages_load_error: Option<String>,
    pub messages_loading: bool,
}

/// Отрисовка центральной области: заголовок канала, кнопки, чат, поле ввода.
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
        on_action,
        messages_load_error,
        messages_loading,
    } = params;

    // ─── Заголовок канала + кнопки (как в Discord) ─────────────────────────
    egui::TopBottomPanel::top("chat_channel_header")
        .exact_height(CHANNEL_HEADER_HEIGHT)
        .show_separator_line(true)
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                // Название канала
                ui.label(
                    egui::RichText::new(channel_name)
                        .size(16.0)
                        .color(theme.text_primary),
                );
                if let Some(desc) = channel_description.filter(|s| !s.is_empty()) {
                    ui.label(
                        egui::RichText::new(desc).small().color(theme.text_muted),
                    );
                }
                ui.add_space(8.0);
                // Кнопки: Threads, Notifications, Pinned, Member list, Search, Inbox, Help
                header_icon_button(ui, theme, "🧵", "Треды", || (*on_action)(ChatPanelAction::Threads));
                header_icon_button(ui, theme, "🔔", "Уведомления", || (*on_action)(ChatPanelAction::Notifications));
                header_icon_button(ui, theme, "📌", "Закреплённые", || (*on_action)(ChatPanelAction::Pinned));
                header_icon_button(ui, theme, "👥", "Список участников", || (*on_action)(ChatPanelAction::ToggleMemberList));
                header_icon_button(ui, theme, "🔍", "Поиск", || (*on_action)(ChatPanelAction::Search));
                header_icon_button(ui, theme, "📥", "Входящие", || (*on_action)(ChatPanelAction::Inbox));
                header_icon_button(ui, theme, "❓", "Справка", || (*on_action)(ChatPanelAction::Help));
            });
        });

    // ─── Область чата (скролл) и под нею поле ввода ───────────────────────
    // Резервируем высоту под поле ввода (индикатор печати + вложение + строка ввода).
    const INPUT_SECTION_HEIGHT: f32 = 120.0;
    let scroll_height = (ui.available_height() - INPUT_SECTION_HEIGHT).max(60.0);
    let chat_width = ui.available_width();

    // Скроллируемый чат (сверху)
    egui::ScrollArea::vertical()
        .id_source("chat_scroll")
        .stick_to_bottom(true)
        .max_height(scroll_height)
        .show(ui, |ui| {
            ui.set_min_width(chat_width);
            if messages_loading {
                ui.vertical_centered(|ui| {
                    ui.add_space(48.0);
                    ui.spinner();
                    ui.label(egui::RichText::new("Загрузка сообщений...").color(theme.text_muted));
                });
                return;
            }
            if let Some(ref err) = messages_load_error {
                ui.vertical_centered(|ui| {
                    ui.add_space(48.0);
                    ui.label(egui::RichText::new(err).color(theme.error));
                    ui.add_space(8.0);
                    if ui.button("Повторить").clicked() {
                        (*on_action)(ChatPanelAction::RetryMessages);
                    }
                });
                return;
            }
            if messages.is_empty() {
                ui.label(
                    egui::RichText::new("Нет сообщений. Напишите первым!")
                        .color(theme.text_muted),
                );
                return;
            }
            let my_uid = current_user_id.unwrap_or(-1);
            let mut prev_author: i64 = -1;
            for msg in messages.iter() {
                let show_header = msg.author_id != prev_author;
                prev_author = msg.author_id;
                if show_header {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&msg.author_username)
                                .strong()
                                .color(theme.text_primary),
                        );
                        ui.label(
                            egui::RichText::new(&msg.created_at)
                                .small()
                                .color(theme.text_muted),
                        );
                    });
                }
                let has_att = !msg.attachments.is_empty();
                let content_visible = !msg.content.is_empty()
                    && !(has_att
                        && msg
                            .attachments
                            .iter()
                            .any(|a| a.filename == msg.content));
                if content_visible {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&msg.content).color(theme.text_primary),
                        );
                        if msg.author_id == my_uid {
                            let seen = !msg.seen_by.is_empty();
                            let mark = if seen { "^^" } else { "^" };
                            let mark_color = if seen {
                                theme.success
                            } else {
                                theme.text_muted
                            };
                            let mark_label = ui.label(
                                egui::RichText::new(mark).small().color(mark_color),
                            );
                            if seen {
                                let seen_names: Vec<String> = server_members
                                    .iter()
                                    .filter(|m| msg.seen_by.contains(&m.user_id))
                                    .map(|m| {
                                        if m.display_name.is_empty() {
                                            m.username.clone()
                                        } else {
                                            m.display_name.clone()
                                        }
                                    })
                                    .collect();
                                mark_label.on_hover_text(format!("Видели: {}", seen_names.join(", ")));
                            }
                        }
                    });
                }
                for att in &msg.attachments {
                    paint_attachment(
                        ui,
                        theme,
                        att,
                        media_textures,
                        media_bytes,
                    );
                }
            }
        });

    // ─── Поле ввода внизу ─────────────────────────────────────────────────
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
    if !typing_users.is_empty() {
        let text = if typing_users.len() == 1 {
            format!("{} печатает...", typing_users[0].1)
        } else {
            format!("{} и ещё {} печатают...", typing_users[0].1, typing_users.len() - 1)
        };
        ui.label(
            egui::RichText::new(text).small().color(theme.text_muted).italics(),
        );
    }
    if let Some(att) = pending_attachment {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("📎 {}", att.filename)).small().color(theme.text_secondary),
            );
            if ui.small_button("✕").on_hover_text("Убрать вложение").clicked() {
                (*on_action)(ChatPanelAction::ClearAttachment);
            }
        });
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .add_sized([ICON_BUTTON_SIZE, INPUT_LINE_HEIGHT], egui::Button::new("📎"))
            .on_hover_text("Прикрепить файл")
            .clicked()
        {
            (*on_action)(ChatPanelAction::AttachRequest);
        }
        if ui
            .add_sized([ICON_BUTTON_SIZE, INPUT_LINE_HEIGHT], egui::Button::new("GIF"))
            .on_hover_text("GIF")
            .clicked()
        {
            (*on_action)(ChatPanelAction::StubGif);
        }
        if ui
            .add_sized([ICON_BUTTON_SIZE, INPUT_LINE_HEIGHT], egui::Button::new("😀"))
            .on_hover_text("Эмодзи")
            .clicked()
        {
            (*on_action)(ChatPanelAction::StubEmoji);
        }
        if ui
            .add_sized([ICON_BUTTON_SIZE, INPUT_LINE_HEIGHT], egui::Button::new("🎴"))
            .on_hover_text("Стикеры")
            .clicked()
        {
            (*on_action)(ChatPanelAction::StubStickers);
        }
        let w = ui.available_width() - (ICON_BUTTON_SIZE * 4.0 + 84.0 + 24.0);
        let input = ui.add_sized(
            [w.max(120.0), INPUT_LINE_HEIGHT],
            egui::TextEdit::singleline(new_message).hint_text("Написать сообщение..."),
        );
        let send_clicked = ui
            .add_sized([84.0, INPUT_LINE_HEIGHT], egui::Button::new("Отправить"))
            .clicked();
        let enter_send = input.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if send_clicked || enter_send {
            (*on_action)(ChatPanelAction::SendMessage);
        }
    });
    ui.add_space(8.0);
}

fn header_icon_button<F: FnOnce()>(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: &str,
    tooltip: &str,
    on_click: F,
) {
    let btn = ui
        .add_sized(
            [ICON_BUTTON_SIZE, ICON_BUTTON_SIZE],
            egui::Button::new(egui::RichText::new(icon).size(14.0).color(theme.text_secondary)),
        )
        .on_hover_text(tooltip);
    if btn.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if btn.clicked() {
        on_click();
    }
}

fn paint_attachment(
    ui: &mut egui::Ui,
    theme: &Theme,
    att: &AttachmentMeta,
    media_textures: &HashMap<i64, egui::TextureHandle>,
    media_bytes: &HashMap<i64, (Vec<u8>, String)>,
) {
    let is_image = att.mime_type.starts_with("image/");
    let is_video = att.mime_type.starts_with("video/");
    let mid = att.media_id;

    if is_image {
        if let Some(tex) = media_textures.get(&mid) {
            let orig = tex.size_vec2();
            let scale = (400.0 / orig.x).min(200.0 / orig.y).min(1.0);
            let disp = egui::vec2(orig.x * scale, orig.y * scale);
            let (rect, _) = ui.allocate_exact_size(disp, egui::Sense::hover());
            let img = egui::Image::new(tex).fit_to_exact_size(disp);
            img.paint_at(ui, rect);
            let btn_rect = egui::Rect::from_min_size(
                rect.right_top() + egui::vec2(-28.0, 2.0),
                egui::vec2(26.0, 22.0),
            );
            if ui
                .put(
                    btn_rect,
                    egui::Button::new("⬇").min_size(egui::vec2(26.0, 22.0)),
                )
                .on_hover_text("Скачать")
                .clicked()
            {
                save_media_to_disk(mid, &att.filename, media_bytes);
            }
        } else {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("🖼 {}", att.filename))
                        .color(theme.text_muted),
                );
                if ui.small_button("⬇").on_hover_text("Скачать").clicked() {
                    save_media_to_disk(mid, &att.filename, media_bytes);
                }
            });
        }
    } else if is_video {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("🎬 {}", att.filename))
                    .color(theme.text_primary),
            );
            if ui.small_button("⬇").on_hover_text("Скачать").clicked() {
                save_media_to_disk(mid, &att.filename, media_bytes);
            }
        });
    } else {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("📁 {}", att.filename))
                    .color(theme.text_primary),
            );
            ui.label(
                egui::RichText::new(format!("({})", fmt_size(att.size_bytes)))
                    .small()
                    .color(theme.text_muted),
            );
            if ui.small_button("⬇").on_hover_text("Скачать").clicked() {
                save_media_to_disk(mid, &att.filename, media_bytes);
            }
        });
    }
}

fn fmt_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{} Б", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} КБ", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} МБ", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn save_media_to_disk(
    media_id: i64,
    filename: &str,
    media_bytes: &HashMap<i64, (Vec<u8>, String)>,
) {
    if let Some((bytes, _)) = media_bytes.get(&media_id) {
        if let Some(save_path) = rfd::FileDialog::new()
            .set_file_name(filename)
            .save_file()
        {
            let _ = std::fs::write(save_path, bytes);
        }
    }
}
