//! Centralized Discord-like theme tokens shared across the native egui UI.

#![allow(dead_code)]

use eframe::egui::{self, Color32, Rounding, Stroke};

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg_primary: Color32,
    pub bg_secondary: Color32,
    pub bg_tertiary: Color32,
    pub bg_quaternary: Color32,
    pub bg_hover: Color32,
    pub bg_active: Color32,
    pub bg_input: Color32,
    pub bg_elevated: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub error: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub text_link: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub online: Color32,
    pub notification: Color32,
}

impl Default for Theme {
    fn default() -> Self {
        Self::discord_dark()
    }
}

impl Theme {
    pub fn discord_dark() -> Self {
        Self {
            bg_primary: Color32::from_rgb(49, 51, 56),
            bg_secondary: Color32::from_rgb(43, 45, 49),
            bg_tertiary: Color32::from_rgb(30, 31, 34),
            bg_quaternary: Color32::from_rgb(35, 36, 40),
            bg_hover: Color32::from_rgb(53, 55, 60),
            bg_active: Color32::from_rgb(64, 66, 73),
            bg_input: Color32::from_rgb(56, 58, 64),
            bg_elevated: Color32::from_rgb(17, 18, 20),
            accent: Color32::from_rgb(88, 101, 242),
            accent_hover: Color32::from_rgb(71, 82, 196),
            error: Color32::from_rgb(237, 66, 69),
            success: Color32::from_rgb(35, 165, 90),
            warning: Color32::from_rgb(250, 166, 26),
            text_primary: Color32::from_rgb(242, 243, 245),
            text_secondary: Color32::from_rgb(219, 222, 225),
            text_muted: Color32::from_rgb(148, 155, 164),
            text_link: Color32::from_rgb(0, 168, 252),
            border: Color32::from_rgb(31, 32, 36),
            border_strong: Color32::from_rgb(24, 25, 28),
            online: Color32::from_rgb(35, 165, 90),
            notification: Color32::from_rgb(237, 66, 69),
        }
    }

    #[allow(dead_code)]
    pub fn discord_light() -> Self {
        Self {
            bg_primary: Color32::from_rgb(255, 255, 255),
            bg_secondary: Color32::from_rgb(242, 243, 245),
            bg_tertiary: Color32::from_rgb(230, 232, 235),
            bg_quaternary: Color32::from_rgb(235, 236, 238),
            bg_hover: Color32::from_rgb(228, 230, 235),
            bg_active: Color32::from_rgb(216, 219, 225),
            bg_input: Color32::from_rgb(235, 236, 238),
            bg_elevated: Color32::from_rgb(248, 249, 250),
            accent: Color32::from_rgb(88, 101, 242),
            accent_hover: Color32::from_rgb(71, 82, 196),
            error: Color32::from_rgb(237, 66, 69),
            success: Color32::from_rgb(59, 165, 93),
            warning: Color32::from_rgb(250, 166, 26),
            text_primary: Color32::from_rgb(35, 39, 42),
            text_secondary: Color32::from_rgb(64, 68, 75),
            text_muted: Color32::from_rgb(114, 118, 125),
            text_link: Color32::from_rgb(0, 122, 204),
            border: Color32::from_rgb(220, 221, 222),
            border_strong: Color32::from_rgb(185, 187, 190),
            online: Color32::from_rgb(59, 165, 93),
            notification: Color32::from_rgb(237, 66, 69),
        }
    }

    pub fn apply_egui_visuals(&self, ctx: &egui::Context) {
        let mut visuals = egui::Visuals::dark();
        let widget_rounding = Rounding::same(6.0);

        visuals.override_text_color = Some(self.text_primary);
        visuals.panel_fill = self.bg_primary;
        visuals.faint_bg_color = self.bg_secondary;
        visuals.extreme_bg_color = self.bg_tertiary;
        visuals.window_fill = self.bg_secondary;
        visuals.window_stroke = Stroke::new(1.0, self.border_strong);
        visuals.widgets.noninteractive.bg_fill = self.bg_primary;
        visuals.widgets.noninteractive.weak_bg_fill = self.bg_primary;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, self.border);
        visuals.widgets.noninteractive.rounding = widget_rounding;
        visuals.widgets.inactive.bg_fill = self.bg_quaternary;
        visuals.widgets.inactive.weak_bg_fill = self.bg_quaternary;
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, self.border);
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, self.text_secondary);
        visuals.widgets.inactive.rounding = widget_rounding;
        visuals.widgets.hovered.bg_fill = self.bg_hover;
        visuals.widgets.hovered.weak_bg_fill = self.bg_hover;
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, self.border_strong);
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, self.text_primary);
        visuals.widgets.hovered.rounding = widget_rounding;
        visuals.widgets.active.bg_fill = self.bg_active;
        visuals.widgets.active.weak_bg_fill = self.bg_active;
        visuals.widgets.active.bg_stroke = Stroke::new(1.0, self.border_strong);
        visuals.widgets.active.fg_stroke = Stroke::new(1.0, self.text_primary);
        visuals.widgets.active.rounding = widget_rounding;
        visuals.widgets.open.bg_fill = self.bg_hover;
        visuals.widgets.open.weak_bg_fill = self.bg_hover;
        visuals.widgets.open.bg_stroke = Stroke::new(1.0, self.border);
        visuals.widgets.open.rounding = widget_rounding;
        visuals.selection.bg_fill = self.accent;
        visuals.selection.stroke = Stroke::new(1.0, self.text_primary);
        visuals.hyperlink_color = self.text_link;
        visuals.code_bg_color = self.bg_input;
        visuals.window_rounding = Rounding::same(10.0);
        ctx.set_visuals(visuals);

        let mut style = (*ctx.style()).clone();
        style.spacing.button_padding = egui::vec2(8.0, 5.0);
        style.spacing.item_spacing = egui::vec2(8.0, 5.0);
        style.spacing.window_margin = egui::Margin::same(8.0);
        ctx.set_style(style);
    }

    #[inline]
    pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
        let t = t.clamp(0.0, 1.0);
        Color32::from_rgb(
            lerp_u8(a.r(), b.r(), t),
            lerp_u8(a.g(), b.g(), t),
            lerp_u8(a.b(), b.b(), t),
        )
    }
}

#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let x = (1.0 - t) * (a as f32) + t * (b as f32);
    x.round().clamp(0.0, 255.0) as u8
}
