//! Состояние приложения для Discord-like UI.
//! Типы домена re-export из `net`; доп. enum и структуры для панелей и загрузки.

#![allow(dead_code)]

use crate::net::{Channel, Member, Message, Server};

// ─── Re-exports и алиасы ─────────────────────────────────────────────────────

/// Сервер (гильдия). Алиас к `Server` из net.
pub type Guild = Server;

/// Участник/пользователь в контексте сервера. Алиас к `Member`.
pub type User = Member;

// ─── Типы для UI ─────────────────────────────────────────────────────────────

/// Тип канала для отображения и фильтрации.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    Text,
    Voice,
    Category,
}

impl ChannelType {
    /// По строке из API (`"text"`, `"voice"` и т.д.).
    pub fn from_api(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "voice" => ChannelType::Voice,
            "category" => ChannelType::Category,
            _ => ChannelType::Text,
        }
    }
}

/// Текущий вид: личные сообщения или сервер.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentView {
    /// Личные сообщения (Home/DMs).
    DMs,
    /// Сервер с указанным id.
    Guild { server_id: i64 },
}

// ─── Состояние загрузки (раздел 8: неблокирующая загрузка) ──────────────────

/// Состояние асинхронной загрузки сущности.
#[derive(Debug, Clone)]
pub enum LoadState<T> {
    Idle,
    Loading,
    Loaded(T),
    Error(String),
}

impl<T> Default for LoadState<T> {
    fn default() -> Self {
        LoadState::Idle
    }
}

impl<T> LoadState<T> {
    pub fn is_loading(&self) -> bool {
        matches!(self, LoadState::Loading)
    }
    pub fn is_loaded(&self) -> bool {
        matches!(self, LoadState::Loaded(_))
    }
    pub fn error_message(&self) -> Option<&str> {
        match self {
            LoadState::Error(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

// ─── Категория каналов (группировка для списка каналов) ─────────────────────

/// Группа каналов под одним заголовком (как в Discord).
#[derive(Debug, Clone)]
pub struct ChannelCategory {
    pub name: String,
    pub channel_ids: Vec<i64>,
}

// ─── Корневое состояние приложения ──────────────────────────────────────────

/// Корневое состояние для Discord-like UI.
/// Используется панелями после этапа 3; пока заполняется моками.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Текущий экран: Auth или Main (для нового UI можно ввести отдельный enum).
    pub screen: AppScreen,
    /// Что показываем: DMs или сервер.
    pub current_view: CurrentView,
    /// Выбранный канал (в контексте текущего сервера).
    pub selected_channel_id: Option<i64>,
    /// Список серверов пользователя.
    pub servers: Vec<Server>,
    /// Каналы текущего сервера (обновляются при смене current_view).
    pub channels: Vec<Channel>,
    /// Категории каналов для текущего сервера (имя → id каналов).
    pub channel_categories: Vec<ChannelCategory>,
    /// Сообщения в выбранном канале.
    pub messages: Vec<Message>,
    /// Участники текущего сервера.
    pub members: Vec<Member>,
    /// ID текущего пользователя.
    pub current_user_id: Option<i64>,
    /// Имя текущего пользователя.
    pub current_username: String,
    /// Состояние загрузки каналов (для неблокирующего UI).
    pub channels_load: LoadState<()>,
    /// Состояние загрузки сообщений.
    pub messages_load: LoadState<()>,
    /// Состояние загрузки участников.
    pub members_load: LoadState<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppScreen {
    #[default]
    Auth,
    Main,
}

impl Default for AppState {
    fn default() -> Self {
        Self::with_mock_data()
    }
}

impl AppState {
    /// Создаёт состояние с моковыми данными для разработки UI.
    pub fn with_mock_data() -> Self {
        let servers = mock_servers();
        let (channels, categories) = mock_channels_and_categories();
        let members = mock_members();
        let messages = mock_messages(&channels, &members);

        // По умолчанию: первый сервер, первый текстовый канал
        let first_server_id = servers.first().map(|s| s.id).unwrap_or(1);
        let text_channel_ids: Vec<i64> = channels
            .iter()
            .filter(|c| c.server_id == first_server_id && c.r#type == "text")
            .map(|c| c.id)
            .collect();
        let selected_channel_id = text_channel_ids.first().copied();

        let channel_categories_for_server: Vec<ChannelCategory> = categories
            .into_iter()
            .filter(|c| c.channel_ids.iter().any(|&id| channels.iter().any(|ch| ch.id == id && ch.server_id == first_server_id)))
            .map(|c| ChannelCategory {
                channel_ids: c.channel_ids.into_iter().filter(|&id| channels.iter().any(|ch| ch.id == id && ch.server_id == first_server_id)).collect(),
                ..c
            })
            .filter(|c| !c.channel_ids.is_empty())
            .collect();

        let channels_for_server: Vec<Channel> = channels
            .iter()
            .filter(|c| c.server_id == first_server_id)
            .cloned()
            .collect();

        let messages_for_channel = selected_channel_id
            .map(|cid| messages.iter().filter(|m| m.channel_id == cid).cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        Self {
            screen: AppScreen::Main,
            current_view: CurrentView::Guild { server_id: first_server_id },
            selected_channel_id,
            servers,
            channels: channels_for_server,
            channel_categories: channel_categories_for_server,
            messages: messages_for_channel,
            members: members.clone(),
            current_user_id: Some(1),
            current_username: "astrix_user".to_string(),
            channels_load: LoadState::Loaded(()),
            messages_load: LoadState::Loaded(()),
            members_load: LoadState::Loaded(()),
        }
    }

    /// Возвращает каналы для выбранного сервера (для смены сервера вызывающий подставит нужный server_id и отфильтрует).
    pub fn channels_for_server(server_id: i64, all_channels: &[Channel]) -> Vec<Channel> {
        all_channels.iter().filter(|c| c.server_id == server_id).cloned().collect()
    }

    /// Возвращает категории для выбранного сервера (из полного списка категорий и каналов).
    pub fn categories_for_server(
        server_id: i64,
        categories: &[ChannelCategory],
        all_channels: &[Channel],
    ) -> Vec<ChannelCategory> {
        let server_channel_ids: std::collections::HashSet<i64> =
            all_channels.iter().filter(|c| c.server_id == server_id).map(|c| c.id).collect();
        categories
            .iter()
            .filter_map(|cat| {
                let ids: Vec<i64> = cat.channel_ids.iter().filter(|id| server_channel_ids.contains(id)).copied().collect();
                if ids.is_empty() {
                    None
                } else {
                    Some(ChannelCategory { name: cat.name.clone(), channel_ids: ids })
                }
            })
            .collect()
    }
}

// ─── Моковые данные ───────────────────────────────────────────────────────

/// 3 сервера для мока.
pub fn mock_servers() -> Vec<Server> {
    vec![
        Server { id: 1, name: "Astrix Dev".to_string(), owner_id: 1 },
        Server { id: 2, name: "Gaming".to_string(), owner_id: 2 },
        Server { id: 3, name: "Art Club".to_string(), owner_id: 3 },
    ]
}

/// Каналы и категории для всех серверов. ID каналов: сервер 1 → 101–108, сервер 2 → 201–207, сервер 3 → 301–306.
pub fn mock_channels_and_categories() -> (Vec<Channel>, Vec<ChannelCategory>) {
    let channels = vec![
        // Сервер 1
        Channel { id: 101, server_id: 1, name: "general".to_string(), r#type: "text".to_string() },
        Channel { id: 102, server_id: 1, name: "random".to_string(), r#type: "text".to_string() },
        Channel { id: 103, server_id: 1, name: "General".to_string(), r#type: "voice".to_string() },
        Channel { id: 104, server_id: 1, name: "dev-talk".to_string(), r#type: "text".to_string() },
        Channel { id: 105, server_id: 1, name: "Voice".to_string(), r#type: "voice".to_string() },
        Channel { id: 106, server_id: 1, name: "announcements".to_string(), r#type: "text".to_string() },
        // Сервер 2
        Channel { id: 201, server_id: 2, name: "general".to_string(), r#type: "text".to_string() },
        Channel { id: 202, server_id: 2, name: "clips".to_string(), r#type: "text".to_string() },
        Channel { id: 203, server_id: 2, name: "Lobby".to_string(), r#type: "voice".to_string() },
        Channel { id: 204, server_id: 2, name: "minecraft".to_string(), r#type: "text".to_string() },
        Channel { id: 205, server_id: 2, name: "Games".to_string(), r#type: "voice".to_string() },
        Channel { id: 206, server_id: 2, name: "off-topic".to_string(), r#type: "text".to_string() },
        Channel { id: 207, server_id: 2, name: "Stream".to_string(), r#type: "voice".to_string() },
        // Сервер 3
        Channel { id: 301, server_id: 3, name: "general".to_string(), r#type: "text".to_string() },
        Channel { id: 302, server_id: 3, name: "showcase".to_string(), r#type: "text".to_string() },
        Channel { id: 303, server_id: 3, name: "Voice".to_string(), r#type: "voice".to_string() },
        Channel { id: 304, server_id: 3, name: "feedback".to_string(), r#type: "text".to_string() },
        Channel { id: 305, server_id: 3, name: "collab".to_string(), r#type: "text".to_string() },
        Channel { id: 306, server_id: 3, name: "Stream".to_string(), r#type: "voice".to_string() },
    ];

    let categories = vec![
        ChannelCategory { name: "General".to_string(), channel_ids: vec![101, 102, 103] },
        ChannelCategory { name: "Development".to_string(), channel_ids: vec![104, 105] },
        ChannelCategory { name: "Info".to_string(), channel_ids: vec![106] },
        ChannelCategory { name: "Chat".to_string(), channel_ids: vec![201, 202, 206] },
        ChannelCategory { name: "Voice".to_string(), channel_ids: vec![203, 205, 207] },
        ChannelCategory { name: "Games".to_string(), channel_ids: vec![204] },
        ChannelCategory { name: "General".to_string(), channel_ids: vec![301, 303] },
        ChannelCategory { name: "Art".to_string(), channel_ids: vec![302, 304, 305, 306] },
    ];

    (channels, categories)
}

/// 10 пользователей (участников) для мока.
pub fn mock_members() -> Vec<Member> {
    vec![
        Member { user_id: 1, username: "astrix_user".to_string(), display_name: String::new(), is_owner: true },
        Member { user_id: 2, username: "alice".to_string(), display_name: "Alice".to_string(), is_owner: false },
        Member { user_id: 3, username: "bob".to_string(), display_name: String::new(), is_owner: false },
        Member { user_id: 4, username: "charlie".to_string(), display_name: "Charlie".to_string(), is_owner: false },
        Member { user_id: 5, username: "diana".to_string(), display_name: String::new(), is_owner: false },
        Member { user_id: 6, username: "eve".to_string(), display_name: "Eve".to_string(), is_owner: false },
        Member { user_id: 7, username: "frank".to_string(), display_name: String::new(), is_owner: false },
        Member { user_id: 8, username: "grace".to_string(), display_name: "Grace".to_string(), is_owner: false },
        Member { user_id: 9, username: "henry".to_string(), display_name: String::new(), is_owner: false },
        Member { user_id: 10, username: "iris".to_string(), display_name: "Iris".to_string(), is_owner: false },
    ]
}

/// 30 сообщений в текстовых каналах (разные авторы, каналы, даты).
pub fn mock_messages(channels: &[Channel], members: &[Member]) -> Vec<Message> {
    let text_channel_ids: Vec<i64> = channels.iter().filter(|c| c.r#type == "text").map(|c| c.id).collect();
    let author_ids: Vec<i64> = members.iter().map(|m| m.user_id).collect();
    let usernames: std::collections::HashMap<i64, String> = members.iter().map(|m| (m.user_id, m.username.clone())).collect();

    let contents = [
        "Hey everyone!",
        "Welcome to the server.",
        "How do I set up the bot?",
        "Check the #announcements channel.",
        "Working on the new UI today.",
        "Discord-like layout looks great.",
        "Anyone for a voice call?",
        "I'll be there in 5.",
        "Thanks for the help!",
        "No problem.",
        "We should add more channels.",
        "Good idea.",
        "What about categories?",
        "Already in the mock data.",
        "Let's test the channel switch.",
        "Switching servers works too.",
        "Mock messages are enough for now.",
        "Ready for the next step.",
        "Theme and state are done.",
        "Panels next.",
        "This is message 21.",
        "And 22.",
        "Almost 30.",
        "Last few.",
        "Message 25.",
        "26.",
        "27.",
        "28.",
        "29.",
        "That's 30 messages.",
    ];

    let mut messages = Vec::with_capacity(30);
    for (i, content) in contents.iter().enumerate() {
        let channel_id = text_channel_ids[i % text_channel_ids.len()];
        let author_id = author_ids[i % author_ids.len()];
        let author_username = usernames.get(&author_id).cloned().unwrap_or_else(|| "user".to_string());
        let day = 1 + (i % 28);
        let created_at = format!("2025-03-{:02}T12:00:00Z", day);
        messages.push(Message {
            id: 1000 + i as i64,
            channel_id,
            author_id,
            author_username,
            content: (*content).to_string(),
            created_at,
            attachments: vec![],
            seen_by: vec![],
        });
    }
    messages
}
