//! Right-side member list.

use std::collections::HashMap;

use eframe::egui;

use crate::theme::Theme;

pub const MEMBER_PANEL_WIDTH: f32 = 240.0;
const MEMBER_ROW_HEIGHT: f32 = 36.0;
const MEMBER_AVATAR_RADIUS: f32 = 14.0;

#[derive(Debug, Clone)]
pub enum MemberPanelAction {
    OpenMemberProfile(i64),
}

#[derive(Clone)]
pub struct MemberSnapshot {
    pub user_id: i64,
    pub display_name: String,
    pub username: String,
    pub is_owner: bool,
    pub online: bool,
}

pub struct MemberPanelParams<'a> {
    pub theme: &'a Theme,
    pub members: &'a [MemberSnapshot],
    pub online_count: usize,
    pub speaking: &'a HashMap<i64, bool>,
    pub avatar_textures: &'a HashMap<i64, egui::TextureHandle>,
    pub on_action: &'a mut dyn FnMut(MemberPanelAction),
}

pub fn show(ctx: &egui::Context, ui: &mut egui::Ui, params: MemberPanelParams<'_>) {
    let MemberPanelParams {
        theme,
        members,
        online_count,
        speaking,
        avatar_textures,
        on_action,
    } = params;

    ui.painter()
        .rect_filled(ui.max_rect(), egui::Rounding::ZERO, theme.bg_secondary);

    let offline_count = members.len().saturating_sub(online_count);

    egui::TopBottomPanel::top("member_panel_header")
        .exact_height(40.0)
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("MEMBERS")
                    .size(11.0)
                    .strong()
                    .color(theme.text_muted),
            );
        });

    egui::ScrollArea::vertical()
        .id_source("member_panel_scroll")
        .show(ui, |ui| {
            if members.is_empty() {
                ui.add_space(16.0);
                ui.label(egui::RichText::new("No members").color(theme.text_muted));
                return;
            }

            section_title(ui, theme, format!("ONLINE - {online_count}"));
            for member in members.iter().filter(|member| member.online) {
                let response = member_row(
                    ctx,
                    ui,
                    theme,
                    member,
                    false,
                    avatar_textures.get(&member.user_id),
                );
                if response.clicked() {
                    (*on_action)(MemberPanelAction::OpenMemberProfile(member.user_id));
                }
            }

            ui.add_space(10.0);
            section_title(ui, theme, format!("OFFLINE - {offline_count}"));
            for member in members.iter().filter(|member| !member.online) {
                let response = member_row(
                    ctx,
                    ui,
                    theme,
                    member,
                    false,
                    avatar_textures.get(&member.user_id),
                );
                if response.clicked() {
                    (*on_action)(MemberPanelAction::OpenMemberProfile(member.user_id));
                }
            }
        });
}

fn section_title(ui: &mut egui::Ui, theme: &Theme, title: String) {
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(title)
            .size(11.0)
            .strong()
            .color(theme.text_muted),
    );
    ui.add_space(4.0);
}

fn member_row(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    theme: &Theme,
    member: &MemberSnapshot,
    _is_speaking: bool,
    avatar_texture: Option<&egui::TextureHandle>,
) -> egui::Response {
    let id = ui.make_persistent_id(("member_row", member.user_id));
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), MEMBER_ROW_HEIGHT),
        egui::Sense::click(),
    );
    let hovered = response.hovered();
    let hover_t = ctx.animate_bool(id.with("hover"), hovered);

    let fill = Theme::lerp_color(theme.bg_secondary, theme.bg_hover, hover_t * 0.18);
    ui.painter()
        .rect_filled(rect, egui::Rounding::same(4.0), fill);

    let inner = rect.shrink2(egui::vec2(8.0, 4.0));
    let avatar_center = egui::pos2(inner.left() + MEMBER_AVATAR_RADIUS, inner.center().y);
    crate::components::avatar::avatar_at(
        ui,
        theme,
        avatar_center,
        &member.display_name,
        MEMBER_AVATAR_RADIUS,
        false,
        avatar_texture,
    );

    ui.painter().circle_filled(
        egui::pos2(
            avatar_center.x + MEMBER_AVATAR_RADIUS + 7.0,
            inner.center().y,
        ),
        3.0,
        if member.online {
            theme.online
        } else {
            theme.text_muted
        },
    );

    let text_x = avatar_center.x + MEMBER_AVATAR_RADIUS + 16.0;
    ui.painter().text(
        egui::pos2(text_x, inner.center().y - 6.0),
        egui::Align2::LEFT_CENTER,
        &member.display_name,
        egui::FontId::proportional(13.0),
        if member.online {
            theme.text_primary
        } else {
            theme.text_secondary
        },
    );

    let secondary = if member.is_owner {
        "Server owner"
    } else {
        member.username.as_str()
    };
    ui.painter().text(
        egui::pos2(text_x, inner.center().y + 8.0),
        egui::Align2::LEFT_CENTER,
        secondary,
        egui::FontId::proportional(10.5),
        theme.text_muted,
    );

    response.on_hover_text("Open member profile")
}
