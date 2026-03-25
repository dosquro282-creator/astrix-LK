//! Тема приложения — цвета в стиле Discord.
//! Все цвета вынесены в один модуль для единообразия и будущей смены темы.

#![allow(dead_code)] // поля и lerp используются панелями на следующих этапах

use eframe::egui::Color32;

/// Цветовая схема приложения (Discord-like).
/// Тёмная тема по умолчанию.
#[derive(Debug, Clone)]
pub struct Theme {
    // --- Фоны ---
    /// Основной фон (глубокий тёмно-серый, как центральная область Discord).
    pub bg_primary: Color32,
    /// Вторичный фон (панели, списки каналов/серверов).
    pub bg_secondary: Color32,
    /// Фон при наведении (элементы списков, кнопки).
    pub bg_hover: Color32,
    /// Фон выбранного/активного элемента (канал, сервер).
    pub bg_active: Color32,

    // --- Акценты ---
    /// Accent — blurple (фиолетово-синий, кнопки, ссылки, индикаторы).
    pub accent: Color32,
    /// Акцент при наведении (чуть светлее).
    pub accent_hover: Color32,

    // --- Семантика ---
    /// Ошибки, опасные действия.
    pub error: Color32,
    /// Успех, подтверждение, онлайн.
    pub success: Color32,
    /// Предупреждение (опционально).
    pub warning: Color32,

    // --- Текст ---
    /// Основной текст.
    pub text_primary: Color32,
    /// Вторичный текст (подписи, мета).
    pub text_secondary: Color32,
    /// Приглушённый текст (неактивные элементы).
    pub text_muted: Color32,

    // --- Границы и разделители ---
    /// Тонкая граница / разделитель.
    pub border: Color32,
    /// Более заметная граница (модалки, фокус).
    pub border_strong: Color32,
}

impl Default for Theme {
    fn default() -> Self {
        Self::discord_dark()
    }
}

impl Theme {
    /// Тёмная тема в стиле Discord (по умолчанию).
    pub fn discord_dark() -> Self {
        Self {
            // Фоны — приближено к Discord (#313338, #2b2d31, #404249)
            bg_primary: Color32::from_rgb(49, 51, 56),   // #313338
            bg_secondary: Color32::from_rgb(43, 45, 49), // #2b2d31
            bg_hover: Color32::from_rgb(64, 66, 73),    // #404249
            bg_active: Color32::from_rgb(64, 66, 73),   // тот же hover для выделения

            // Blurple и вариант при hover
            accent: Color32::from_rgb(88, 101, 242),    // #5865F2
            accent_hover: Color32::from_rgb(114, 127, 250),

            error: Color32::from_rgb(237, 66, 69),      // #ED4245
            success: Color32::from_rgb(87, 242, 135),  // #57F287
            warning: Color32::from_rgb(250, 166, 26),  // #FAA61A

            text_primary: Color32::from_rgb(242, 243, 245),  // #F2F3F5
            text_secondary: Color32::from_rgb(181, 186, 191), // #B5BAC1
            text_muted: Color32::from_rgb(128, 132, 142),     // #80888E

            border: Color32::from_rgb(64, 68, 75),
            border_strong: Color32::from_rgb(78, 80, 88),
        }
    }

    /// Светлая тема (на будущее, для переключения).
    #[allow(dead_code)]
    pub fn discord_light() -> Self {
        Self {
            bg_primary: Color32::from_rgb(255, 255, 255),
            bg_secondary: Color32::from_rgb(242, 243, 245),
            bg_hover: Color32::from_rgb(228, 230, 235),
            bg_active: Color32::from_rgb(228, 230, 235),

            accent: Color32::from_rgb(88, 101, 242),
            accent_hover: Color32::from_rgb(71, 82, 196),

            error: Color32::from_rgb(237, 66, 69),
            success: Color32::from_rgb(59, 165, 93),
            warning: Color32::from_rgb(250, 166, 26),

            text_primary: Color32::from_rgb(35, 39, 42),
            text_secondary: Color32::from_rgb(64, 68, 75),
            text_muted: Color32::from_rgb(114, 118, 125),

            border: Color32::from_rgb(220, 221, 222),
            border_strong: Color32::from_rgb(185, 187, 190),
        }
    }

    /// Линейная интерполяция между двумя цветами (для плавных переходов темы).
    /// `t = 0` → `a`, `t = 1` → `b`.
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
