//! Левая колонка UI: Home/DMs, список серверов, Add Server, Explore Servers.
//! Discord-like: круглые иконки, анимации hover, вертикальная полоска у активного сервера.

use eframe::egui;

use crate::components::buttons;
use crate::net::Server;
use crate::theme::Theme;

/// Ширина левой панели (как в Discord).
pub const GUILD_PANEL_WIDTH: f32 = 72.0;

/// Действие пользователя в панели серверов (один колбэк вместо нескольких заимствований).
#[derive(Debug, Clone, Copy)]
pub enum GuildPanelAction {
    SelectDms,
    SelectServer(i64),
    AddServer,
    Explore,
    DeleteServer(i64),
    ThemeToggle,
    RetryServers,
}

/// Параметры для отрисовки панели серверов.
pub struct GuildPanelParams<'a> {
    pub theme: &'a Theme,
    pub servers: &'a [Server],
    /// Текущий выбор: `None` = Home/DMs, `Some(id)` = сервер.
    pub selected_server: Option<i64>,
    /// Единый колбэк при любом действии (переключение сервера, кнопки, тема).
    pub on_action: &'a mut dyn FnMut(GuildPanelAction),
    pub dark_mode: bool,
    pub servers_loading: bool,
    pub servers_error: Option<&'a str>,
}

/// Отрисовка левой колонки: Home, серверы, Add, Explore, переключатель темы внизу.
pub fn show(ctx: &egui::Context, ui: &mut egui::Ui, params: GuildPanelParams<'_>) {
    let GuildPanelParams {
        theme,
        servers,
        selected_server,
        on_action,
        dark_mode,
        servers_loading,
        servers_error,
    } = params;

    let icon_size = 24.0_f32 * 2.0 + 24.0;
    let scroll_h = (ui.available_height() - icon_size).max(40.0);

    egui::ScrollArea::vertical()
        .id_source("guild_panel_scroll")
        .max_height(scroll_h)
        .show(ui, |ui| {
            ui.add_space(8.0);

            // ─── Home / DMs ───────────────────────────────────────────────────
            let dms_selected = selected_server.is_none();
            let resp = buttons::icon_circle(
                ctx,
                ui,
                theme,
                "⌂",
                "Home",
                dms_selected,
                "guild_home",
            );
            if resp.clicked() {
                (*on_action)(GuildPanelAction::SelectDms);
            }
            ui.add_space(4.0);

            // ─── Разделитель (тонкая линия между Home и серверами) ───────────
            ui.add_space(4.0);
            let sep_rect = ui.allocate_exact_size(
                egui::vec2(GUILD_PANEL_WIDTH - 16.0, 1.0),
                egui::Sense::hover(),
            ).0;
            ui.painter().rect_filled(sep_rect, 0.0, theme.border);
            ui.add_space(8.0);

            if servers_loading && servers.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.spinner();
                });
            } else if let Some(err) = servers_error {
                if servers.is_empty() {
                    let resp = buttons::icon_circle(ctx, ui, theme, "↻", err, false, "guild_retry");
                    if resp.clicked() {
                        (*on_action)(GuildPanelAction::RetryServers);
                    }
                }
            }

            // ─── Список серверов ─────────────────────────────────────────────
            for server in servers.iter() {
                let letter = server
                    .name
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".to_string());
                let sel = selected_server == Some(server.id);
                let resp = buttons::icon_circle(
                    ctx,
                    ui,
                    theme,
                    &letter,
                    &server.name,
                    sel,
                    format!("guild_srv_{}", server.id),
                );
                if resp.clicked() {
                    (*on_action)(GuildPanelAction::SelectServer(server.id));
                }
                resp.context_menu(|ui| {
                    if ui.button("🗑 Удалить / Покинуть").clicked() {
                        (*on_action)(GuildPanelAction::DeleteServer(server.id));
                        ui.close_menu();
                    }
                });
                ui.add_space(4.0);
            }

            // ─── Add Server ─────────────────────────────────────────────────
            let resp = buttons::icon_circle(ctx, ui, theme, "+", "Создать сервер", false, "guild_add");
            if resp.clicked() {
                (*on_action)(GuildPanelAction::AddServer);
            }
            ui.add_space(4.0);

            // ─── Explore Servers ─────────────────────────────────────────────
            let resp = buttons::icon_circle(ctx, ui, theme, "◇", "Explore Servers", false, "guild_explore");
            if resp.clicked() {
                (*on_action)(GuildPanelAction::Explore);
            }
        });

    // ─── Нижняя зона: переключатель темы ─────────────────────────────────────
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
        let icon = if dark_mode { "☀" } else { "🌙" };
        let tip = if dark_mode { "Светлая тема" } else { "Тёмная тема" };
        let resp = buttons::icon_circle(ctx, ui, theme, icon, tip, dark_mode, "guild_theme");
        if resp.clicked() {
            (*on_action)(GuildPanelAction::ThemeToggle);
        }
    });
}
