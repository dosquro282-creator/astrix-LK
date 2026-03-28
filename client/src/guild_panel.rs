//! Far-left server rail.

use eframe::egui;

use crate::bottom_panel;
use crate::components::buttons;
use crate::net::Server;
use crate::theme::Theme;

pub const GUILD_PANEL_WIDTH: f32 = 72.0;

#[derive(Debug, Clone, Copy)]
pub enum GuildPanelAction {
    SelectDms,
    SelectServer(i64),
    AddServer,
    Explore,
    DeleteServer(i64),
    RetryServers,
}

pub struct GuildPanelParams<'a> {
    pub theme: &'a Theme,
    pub servers: &'a [Server],
    pub selected_server: Option<i64>,
    pub on_action: &'a mut dyn FnMut(GuildPanelAction),
    pub servers_loading: bool,
    pub servers_error: Option<&'a str>,
}

pub fn show(ctx: &egui::Context, ui: &mut egui::Ui, params: GuildPanelParams<'_>) {
    let GuildPanelParams {
        theme,
        servers,
        selected_server,
        on_action,
        servers_loading,
        servers_error,
    } = params;

    ui.painter()
        .rect_filled(ui.max_rect(), egui::Rounding::ZERO, theme.bg_tertiary);

    egui::TopBottomPanel::bottom("guild_panel_footer")
        .exact_height(bottom_panel::BOTTOM_PANEL_HEIGHT)
        .show_separator_line(false)
        .show_inside(ui, |_ui| {});

    egui::ScrollArea::vertical()
        .id_source("guild_panel_scroll")
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 8.0);
            ui.vertical_centered(|ui| {
                ui.add_space(12.0);

                let home = buttons::icon_circle(
                    ctx,
                    ui,
                    theme,
                    "A",
                    "Home",
                    selected_server.is_none(),
                    "guild_home",
                );
                if home.clicked() {
                    (*on_action)(GuildPanelAction::SelectDms);
                }

                ui.add_space(8.0);
                let sep_rect = ui
                    .allocate_exact_size(egui::vec2(32.0, 2.0), egui::Sense::hover())
                    .0;
                ui.painter()
                    .rect_filled(sep_rect, 1.0, theme.bg_active.linear_multiply(0.75));
                ui.add_space(8.0);

                if servers_loading && servers.is_empty() {
                    ui.spinner();
                    ui.add_space(8.0);
                }

                if let Some(err) = servers_error.filter(|_| servers.is_empty()) {
                    let retry =
                        buttons::icon_circle(ctx, ui, theme, "!", err, false, "guild_retry");
                    if retry.clicked() {
                        (*on_action)(GuildPanelAction::RetryServers);
                    }
                    ui.add_space(8.0);
                }

                for server in servers {
                    let glyph = server
                        .name
                        .split_whitespace()
                        .take(2)
                        .filter_map(|segment| segment.chars().next())
                        .collect::<String>()
                        .chars()
                        .take(2)
                        .collect::<String>()
                        .to_uppercase();
                    let response = buttons::icon_circle(
                        ctx,
                        ui,
                        theme,
                        if glyph.is_empty() {
                            "?"
                        } else {
                            glyph.as_str()
                        },
                        &server.name,
                        selected_server == Some(server.id),
                        format!("guild_srv_{}", server.id),
                    );
                    if response.clicked() {
                        (*on_action)(GuildPanelAction::SelectServer(server.id));
                    }
                    response.context_menu(|ui| {
                        if ui.button("Delete / Leave server").clicked() {
                            (*on_action)(GuildPanelAction::DeleteServer(server.id));
                            ui.close_menu();
                        }
                    });
                    ui.add_space(8.0);
                }

                let add =
                    buttons::icon_circle(ctx, ui, theme, "+", "Create server", false, "guild_add");
                if add.clicked() {
                    (*on_action)(GuildPanelAction::AddServer);
                }
                ui.add_space(8.0);

                let explore = buttons::icon_circle(
                    ctx,
                    ui,
                    theme,
                    "o",
                    "Explore servers",
                    false,
                    "guild_explore",
                );
                if explore.clicked() {
                    (*on_action)(GuildPanelAction::Explore);
                }
                ui.add_space(8.0);
            });
        });
}
