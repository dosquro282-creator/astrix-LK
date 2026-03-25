use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Tracks rendered frames per stream for display FPS (frames actually drawn per second).
#[derive(Clone)]
pub(crate) struct RenderFpsTracker {
    count: u32,
    window_start: Instant,
    last_fps: f32,
}

impl Default for RenderFpsTracker {
    fn default() -> Self {
        Self {
            count: 0,
            window_start: Instant::now(),
            last_fps: 0.0,
        }
    }
}

impl RenderFpsTracker {
    fn record(&mut self) {
        self.count += 1;
    }

    /// Update FPS from sliding 1s window, return last known FPS.
    fn update_and_get(&mut self) -> f32 {
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.last_fps = self.count as f32 / elapsed.as_secs_f32();
            self.count = 0;
            self.window_start = Instant::now();
        }
        self.last_fps
    }
}

use eframe::egui;
use parking_lot::Mutex;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use egui_glow;

use crate::crypto::ChannelKey;
use crate::net::{
    ApiClient, AttachmentMeta, Channel, LoginRequest, Member, Message,
    RegisterRequest, Server, VoiceParticipant,
    WsClientMsg, WsEventQueue, new_event_queue, ws_task,
};
use crate::channel_panel::{
    self, ChannelPanelAction, ChannelPanelParams, ChannelPanelVoiceSnapshot,
    ChannelsLoadState,
};
use crate::chat_panel::{self, ChatPanelAction, ChatPanelParams};
use crate::guild_panel::{self, GuildPanelParams};
use crate::member_panel::{self, MemberPanelParams, MemberSnapshot};
use crate::theme::Theme;
use crate::telemetry::PipelineTelemetry;
use crate::voice::{VoiceCmd, VideoFrame, VideoFrames, VoiceSessionStats, video_frame_key, spawn_voice_engine};

// ─── Persistent settings (saved to disk) ────────────────────────────────────
// Used by app.rs; path and struct duplicated there for now.

const SETTINGS_PATH: &str = "astrix_settings.json";

fn default_api_base() -> String {
    crate::net::DEFAULT_API_BASE.to_string()
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct Settings {
    pub(crate) remember_me: bool,
    pub(crate) saved_username: String,
    pub(crate) saved_password: String,
    #[serde(default = "default_api_base")]
    pub(crate) api_base: String,
    pub(crate) last_server: HashMap<String, i64>,
    #[serde(default)]
    pub(crate) dark_mode: bool,
    #[serde(default)]
    pub(crate) voice_volume_by_user: HashMap<String, f32>,
    /// Путь декодирования входящего видео: "cpu" (OpenH264) или "mft" (Media Foundation).
    #[serde(default)]
    pub(crate) decode_path: String,
    /// Gamma для GPU-декодера (MFT path): pow(rgb, 1/gamma). 0 = выкл. 0.55 ≈ корректно. Диапазон 0..3.
    #[serde(default = "default_video_decoder_gamma")]
    pub(crate) video_decoder_gamma: f32,
}

fn default_video_decoder_gamma() -> f32 {
    0.55
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            remember_me: false,
            saved_username: String::new(),
            saved_password: String::new(),
            api_base: default_api_base(),
            last_server: HashMap::new(),
            dark_mode: false,
            voice_volume_by_user: HashMap::new(),
            decode_path: String::new(),
            video_decoder_gamma: 0.55,
        }
    }
}

impl Settings {
    pub(crate) fn load() -> Self {
        let mut s = if let Ok(s) = std::fs::read_to_string(SETTINGS_PATH) {
            serde_json::from_str(&s).unwrap_or_default()
        } else {
            Self::default()
        };
        if s.decode_path.is_empty() || (s.decode_path != "cpu" && s.decode_path != "mft") {
            s.decode_path = "mft".to_string();
        }
        if s.api_base.trim().is_empty() {
            s.api_base = default_api_base();
        }
        s.video_decoder_gamma = s.video_decoder_gamma.clamp(0.0, 3.0);
        #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
        crate::d3d11_rgba::set_video_decoder_gamma(s.video_decoder_gamma);
        s
    }
    pub(crate) fn save(&self) {
        let _ = std::fs::write(SETTINGS_PATH, serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

// ─── State (used by app.rs) ──────────────────────────────────────────────────

pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

pub(crate) fn find_attachment_mime(messages: &[Message], media_id: i64) -> String {
    for msg in messages {
        for att in &msg.attachments {
            if att.media_id == media_id {
                return att.mime_type.clone();
            }
        }
    }
    "application/octet-stream".to_string()
}

// (eframe::App impl moved to app.rs)

// ─── Load state (неблокирующая загрузка, раздел 8) ─────────────────────────────

/// Состояние загрузки для каналов, участников, сообщений.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LoadState {
    Idle,
    Loading,
    Loaded,
    Error(String),
}

impl Default for LoadState {
    fn default() -> Self {
        LoadState::Idle
    }
}

/// Таймаут для фоновых загрузок (секунды).
const LOAD_TIMEOUT_SECS: u64 = 15;

/// Запускает фоновые задачи загрузки. Вызывается из app::update() перед main_screen.
/// Не блокирует UI; при смене сервера/канала ставит Loading и spawn'ит tokio-задачу.
pub(crate) fn process_background_loads(
    ctx: egui::Context,
    state: Arc<Mutex<State>>,
    api: ApiClient,
) {
    use std::time::Duration;

    let (do_servers, do_channels, do_messages, token, server_id, channel_id, user_id, last_server) = {
        let st = state.lock();
        if st.screen != Screen::Main {
            return;
        }
        let token = match &st.access_token {
            Some(t) => t.clone(),
            None => return,
        };
        let do_servers = st.main.servers_load == LoadState::Idle || st.main.retry_servers;
        let do_channels = (st.main.channels_load == LoadState::Idle && st.main.selected_server.is_some())
            || st.main.retry_channels
            || (st.main.selected_server != st.main.channels_load_for && st.main.selected_server.is_some());
        let do_messages = (st.main.messages_load == LoadState::Idle && st.main.selected_channel.is_some())
            || st.main.retry_messages
            || (st.main.selected_channel != st.main.messages_load_for && st.main.selected_channel.is_some());
        let server_id = st.main.selected_server;
        let channel_id = st.main.selected_channel;
        let user_id = st.user_id;
        let last_server = st.settings.last_server.clone();
        (do_servers, do_channels, do_messages, token, server_id, channel_id, user_id, last_server)
    };

    if do_servers {
        {
            let mut st = state.lock();
            st.main.servers_load = LoadState::Loading;
            st.main.retry_servers = false;
        }
        let state_c = Arc::clone(&state);
        let api_c = api.clone();
        let ctx_c = ctx.clone();
        let token_servers = token.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(LOAD_TIMEOUT_SECS),
                api_c.list_servers(&token_servers),
            )
            .await;
            let mut st = state_c.lock();
            // Игнорируем результат, если пользователь вышел пока загружали
            if st.screen != Screen::Main {
                return;
            }
            match result {
                Ok(Ok(servers)) => {
                    if let Some(uid) = user_id {
                        let key = uid.to_string();
                        if let Some(sid) = last_server.get(&key).copied() {
                            if servers.iter().any(|s| s.id == sid) {
                                st.main.selected_server = Some(sid);
                            }
                        }
                    }
                    st.main.servers = servers;
                    st.main.servers_load = LoadState::Loaded;
                }
                Ok(Err(e)) => st.main.servers_load = LoadState::Error(e.to_string()),
                Err(_) => st.main.servers_load = LoadState::Error("Таймаут загрузки серверов".to_string()),
            }
            ctx_c.request_repaint();
        });
    }

    if do_channels {
        let Some(sid) = server_id else { return };
        {
            let mut st = state.lock();
            st.main.channels_load_for = Some(sid);
            st.main.channels_load = LoadState::Loading;
            st.main.retry_channels = false;
            st.main.channels.clear();
            st.main.server_members.clear();
            st.main.channel_voice.clear();
            st.main.selected_channel = None;
        }
        let state_c = Arc::clone(&state);
        let api_c = api.clone();
        let ctx_c = ctx.clone();
        let token_c = token.clone();
        let user_id_c = user_id;
        tokio::spawn(async move {
            let chs_fut = api_c.list_channels(&token_c, sid);
            let chs_result = tokio::time::timeout(Duration::from_secs(LOAD_TIMEOUT_SECS), chs_fut).await;
            {
                let st = state_c.lock();
                if st.main.selected_server != Some(sid) {
                    return; // Пользователь переключил сервер
                }
            }
            let channels = match chs_result {
                Ok(Ok(chs)) => chs,
                Ok(Err(e)) => {
                    let mut st = state_c.lock();
                    st.main.channels_load = LoadState::Error(e.to_string());
                    ctx_c.request_repaint();
                    return;
                }
                Err(_) => {
                    let mut st = state_c.lock();
                    st.main.channels_load = LoadState::Error("Таймаут загрузки каналов".to_string());
                    ctx_c.request_repaint();
                    return;
                }
            };
            let voice_ch_ids: Vec<i64> = channels.iter()
                .filter(|c| c.r#type == "voice")
                .map(|c| c.id)
                .collect();
            let mut channel_voice = HashMap::new();
            for ch_id in voice_ch_ids {
                if let Ok(Ok(ps)) = tokio::time::timeout(
                    Duration::from_secs(5),
                    api_c.voice_state(&token_c, ch_id),
                ).await {
                    if !ps.is_empty() {
                        channel_voice.insert(ch_id, ps);
                    }
                }
            }
            let ms_fut = api_c.list_server_members(&token_c, sid);
            let ms_result = tokio::time::timeout(Duration::from_secs(LOAD_TIMEOUT_SECS), ms_fut).await;
            let members = match ms_result {
                Ok(Ok(ms)) => ms,
                Ok(Err(e)) => {
                    let mut st = state_c.lock();
                    st.main.channels = channels;
                    st.main.channel_voice = channel_voice;
                    st.main.channels_load = LoadState::Error(format!("Ошибка участников: {}", e));
                    ctx_c.request_repaint();
                    return;
                }
                Err(_) => {
                    let mut st = state_c.lock();
                    st.main.channels = channels;
                    st.main.channel_voice = channel_voice;
                    st.main.channels_load = LoadState::Error("Таймаут загрузки участников".to_string());
                    ctx_c.request_repaint();
                    return;
                }
            };
            let mut st = state_c.lock();
            st.main.channels = channels;
            st.main.server_members = members;
            st.main.channel_voice = channel_voice;
            if let Some(uid) = user_id_c {
                if let Some(me) = st.main.server_members.iter().find(|m| m.user_id == uid) {
                    st.main.my_display_name = me.display_name.clone();
                }
            }
            st.main.channels_load = LoadState::Loaded;
            ctx_c.request_repaint();
        });
    }

    if do_messages {
        let Some(cid) = channel_id else { return };
        let is_text = {
            let st = state.lock();
            st.main.channels.iter()
                .find(|c| c.id == cid)
                .map(|c| c.r#type == "text")
                .unwrap_or(false)
        };
        if !is_text {
            return;
        }
        {
            let mut st = state.lock();
            st.main.messages_load_for = Some(cid);
            st.main.messages_load = LoadState::Loading;
            st.main.retry_messages = false;
            st.main.messages.clear();
        }
        let state_c = Arc::clone(&state);
        let api_c = api.clone();
        let ctx_c = ctx.clone();
        let token_c = token.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(LOAD_TIMEOUT_SECS),
                api_c.list_messages(&token_c, cid),
            )
            .await;
            let mut st = state_c.lock();
            if st.main.selected_channel != Some(cid) {
                return; // Пользователь переключил канал
            }
            match result {
                Ok(Ok(msgs)) => {
                    let max_id = msgs.iter().map(|m| m.id).max().unwrap_or(0);
                    st.main.messages = msgs;
                    if max_id > 0 {
                        st.main.pending_read_receipt = Some((cid, max_id));
                    }
                    st.main.pending_media_ids = st.main.messages.iter()
                        .flat_map(|m| m.attachments.iter().map(|a| a.media_id))
                        .collect();
                    st.main.messages_load = LoadState::Loaded;
                }
                Ok(Err(e)) => st.main.messages_load = LoadState::Error(e.to_string()),
                Err(_) => st.main.messages_load = LoadState::Error("Таймаут загрузки сообщений".to_string()),
            }
            ctx_c.request_repaint();
        });
    }
}

// ─── State (pub(crate) for app.rs) ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Screen { Auth, Main }
impl Default for Screen { fn default() -> Self { Screen::Auth } }

#[derive(Debug, Clone, Default)]
pub(crate) struct AuthState {
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) is_register: bool,
    pub(crate) remember_me: bool,
    pub(crate) error: Option<String>,
}

/// Voice channel presence state (client-side).
#[derive(Debug, Clone)]
pub(crate) struct VoiceState {
    pub(crate) channel_id: Option<i64>,
    pub(crate) server_id: Option<i64>,
    pub(crate) participants: Vec<VoiceParticipant>,
    pub(crate) mic_muted: bool,
    pub(crate) output_muted: bool,
    pub(crate) local_volumes: HashMap<i64, f32>,
    pub(crate) locally_muted: HashSet<i64>,
    pub(crate) speaking: Arc<Mutex<HashMap<i64, bool>>>,
    pub(crate) input_volume: f32,
    pub(crate) output_volume: f32,
    pub(crate) camera_on: bool,
    pub(crate) screen_on: bool,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            channel_id:   None,
            server_id:    None,
            participants: Vec::new(),
            mic_muted:    false,
            output_muted: false,
            local_volumes: HashMap::new(),
            locally_muted: HashSet::new(),
            speaking:      Arc::new(Mutex::new(HashMap::new())),
            input_volume:  2.0,
            output_volume: 2.0,
            camera_on:     false,
            screen_on:     false,
        }
    }
}

pub(crate) struct MainState {
    pub(crate) servers: Vec<Server>,
    pub(crate) channels: Vec<Channel>,
    pub(crate) messages: Vec<Message>,
    pub(crate) server_members: Vec<Member>,
    pub(crate) selected_server: Option<i64>,
    pub(crate) selected_channel: Option<i64>,
    pub(crate) new_message: String,
    pub(crate) prev_message: String,
    pub(crate) new_server_name: String,
    pub(crate) servers_load: LoadState,
    pub(crate) channels_load_for: Option<i64>,
    pub(crate) channels_load: LoadState,
    pub(crate) messages_load_for: Option<i64>,
    pub(crate) messages_load: LoadState,
    pub(crate) retry_servers: bool,
    pub(crate) retry_channels: bool,
    pub(crate) retry_messages: bool,
    pub(crate) show_create_server_dialog: bool,
    pub(crate) show_create_channel_dialog: bool,
    pub(crate) show_invite_dialog: bool,
    pub(crate) show_settings_dialog: bool,
    pub(crate) new_channel_name: String,
    pub(crate) new_channel_is_voice: bool,
    pub(crate) current_channel_key: Option<ChannelKey>,
    pub(crate) server_to_delete: Option<i64>,
    pub(crate) invite_user_id_input: String,
    pub(crate) invite_msg: Option<String>,
    pub(crate) ws_connected_server: Option<i64>,
    pub(crate) ws_viewing_channel: Option<i64>,
    pub(crate) typing_users: Vec<(i64, String, Instant)>,
    pub(crate) last_typing_sent: Option<Instant>,
    pub(crate) online_users: HashSet<i64>,
    pub(crate) my_display_name: String,
    pub(crate) settings_nickname_input: String,
    pub(crate) settings_avatar_path: Option<PathBuf>,
    pub(crate) settings_msg: Option<String>,
    pub(crate) channel_rename: Option<(i64, String)>,
    pub(crate) pending_attachment: Option<AttachmentMeta>,
    pub(crate) pending_attachment_bytes: Option<(Vec<u8>, String)>,
    pub(crate) voice: VoiceState,
    pub(crate) channel_voice: HashMap<i64, Vec<VoiceParticipant>>,
    pub(crate) pending_read_receipt: Option<(i64, i64)>,
    pub(crate) pending_media_ids: Vec<i64>,
    pub(crate) voice_switch_confirm: Option<(i64, i64)>,
    pub(crate) voice_ctx_menu_user: Option<i64>,
    pub(crate) voice_video_textures: HashMap<i64, egui::TextureHandle>,
    /// Phase 3.5: GPU zero-copy textures via WGL_NV_DX_interop2.
    /// key → (egui TextureId, raw GL tex id, width, height).
    /// The TextureId is obtained via frame.register_native_glow_texture() and is what
    /// egui_glow's painter uses to bind the GL texture during rendering.
    pub(crate) voice_video_gpu_textures: HashMap<i64, (egui::TextureId, u32, u32, u32)>,
    /// Registered TextureIds pending deletion (on voice leave).
    pub(crate) voice_video_gpu_tex_pending_delete: Vec<egui::TextureId>,
    pub(crate) voice_pending_leave: bool,
    pub(crate) fullscreen_stream_user: Option<i64>,
    /// Debounce: stream keys seen as non-streaming in previous frame. Remove texture only after 2 consecutive frames.
    pub(crate) stream_ended_prev_frame: HashSet<i64>,
    pub(crate) show_screen_source_picker: bool,
    pub(crate) screen_source_names: Vec<String>,
    pub(crate) screen_preset: crate::voice::ScreenPreset,
    pub(crate) show_voice_stats_window: bool,
    pub(crate) voice_stats: Option<Arc<Mutex<VoiceSessionStats>>>,
    /// Receiver + GUI telemetry. Updated by stream task (render) and UI (gui_draw).
    pub(crate) voice_receiver_telemetry: Option<Arc<PipelineTelemetry>>,
    /// Last time we printed receiver telemetry (from UI, so gui_draw is fresh).
    pub(crate) voice_telemetry_print_at: Option<Instant>,
    /// FPS отрисованных кадров (сколько кадров в секунду реально отрисовалось) по stream_key.
    pub(crate) voice_render_fps: HashMap<i64, RenderFpsTracker>,
    /// Показывать панель участников справа. None = по умолчанию видна, Some(b) = пользователь переключил.
    pub(crate) show_member_panel: Option<bool>,
}

impl Clone for MainState {
    fn clone(&self) -> Self {
        Self {
            servers: self.servers.clone(),
            channels: self.channels.clone(),
            messages: self.messages.clone(),
            server_members: self.server_members.clone(),
            selected_server: self.selected_server,
            selected_channel: self.selected_channel,
            new_message: self.new_message.clone(),
            prev_message: self.prev_message.clone(),
            new_server_name: self.new_server_name.clone(),
            servers_load: self.servers_load.clone(),
            channels_load_for: self.channels_load_for,
            channels_load: self.channels_load.clone(),
            messages_load_for: self.messages_load_for,
            messages_load: self.messages_load.clone(),
            retry_servers: self.retry_servers,
            retry_channels: self.retry_channels,
            retry_messages: self.retry_messages,
            show_create_server_dialog: self.show_create_server_dialog,
            show_create_channel_dialog: self.show_create_channel_dialog,
            show_invite_dialog: self.show_invite_dialog,
            show_settings_dialog: self.show_settings_dialog,
            new_channel_name: self.new_channel_name.clone(),
            new_channel_is_voice: self.new_channel_is_voice,
            current_channel_key: self.current_channel_key.clone(),
            server_to_delete: self.server_to_delete,
            invite_user_id_input: self.invite_user_id_input.clone(),
            invite_msg: self.invite_msg.clone(),
            ws_connected_server: self.ws_connected_server,
            ws_viewing_channel: self.ws_viewing_channel,
            typing_users: self.typing_users.clone(),
            last_typing_sent: self.last_typing_sent,
            online_users: self.online_users.clone(),
            my_display_name: self.my_display_name.clone(),
            settings_nickname_input: self.settings_nickname_input.clone(),
            settings_avatar_path: self.settings_avatar_path.clone(),
            settings_msg: self.settings_msg.clone(),
            channel_rename: self.channel_rename.clone(),
            pending_attachment: self.pending_attachment.clone(),
            pending_attachment_bytes: self.pending_attachment_bytes.clone(),
            pending_read_receipt: self.pending_read_receipt,
            pending_media_ids: self.pending_media_ids.clone(),
            voice: self.voice.clone(),
            channel_voice: self.channel_voice.clone(),
            voice_switch_confirm: self.voice_switch_confirm,
            voice_ctx_menu_user: self.voice_ctx_menu_user,
            voice_video_textures: HashMap::new(),
            voice_video_gpu_textures: HashMap::new(),
            voice_video_gpu_tex_pending_delete: Vec::new(),
            voice_pending_leave: false,
            fullscreen_stream_user: None,
            stream_ended_prev_frame: HashSet::new(),
            show_screen_source_picker: false,
            screen_source_names: Vec::new(),
            screen_preset: crate::voice::ScreenPreset::default(),
            show_voice_stats_window: self.show_voice_stats_window,
            voice_stats: self.voice_stats.clone(), // Arc clone
            voice_receiver_telemetry: self.voice_receiver_telemetry.clone(),
            voice_telemetry_print_at: None, // reset on clone
            voice_render_fps: HashMap::new(),
            show_member_panel: self.show_member_panel,
        }
    }
}

impl Default for MainState {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            channels: Vec::new(),
            messages: Vec::new(),
            server_members: Vec::new(),
            selected_server: None,
            selected_channel: None,
            new_message: String::new(),
            prev_message: String::new(),
            new_server_name: String::new(),
            servers_load: LoadState::Idle,
            channels_load_for: None,
            channels_load: LoadState::Idle,
            messages_load_for: None,
            messages_load: LoadState::Idle,
            retry_servers: false,
            retry_channels: false,
            retry_messages: false,
            show_create_server_dialog: false,
            show_create_channel_dialog: false,
            show_invite_dialog: false,
            show_settings_dialog: false,
            new_channel_name: String::new(),
            new_channel_is_voice: false,
            current_channel_key: None,
            server_to_delete: None,
            invite_user_id_input: String::new(),
            invite_msg: None,
            ws_connected_server: None,
            ws_viewing_channel: None,
            typing_users: Vec::new(),
            last_typing_sent: None,
            online_users: HashSet::new(),
            my_display_name: String::new(),
            settings_nickname_input: String::new(),
            settings_avatar_path: None,
            settings_msg: None,
            channel_rename: None,
            pending_attachment: None,
            pending_attachment_bytes: None,
            voice: VoiceState::default(),
            channel_voice: HashMap::new(),
            pending_read_receipt: None,
            pending_media_ids: Vec::new(),
            voice_switch_confirm: None,
            voice_ctx_menu_user: None,
            voice_video_textures: HashMap::new(),
            voice_video_gpu_textures: HashMap::new(),
            voice_video_gpu_tex_pending_delete: Vec::new(),
            voice_pending_leave: false,
            fullscreen_stream_user: None,
            stream_ended_prev_frame: HashSet::new(),
            show_screen_source_picker: false,
            screen_source_names: Vec::new(),
            screen_preset: crate::voice::ScreenPreset::default(),
            show_voice_stats_window: false,
            voice_stats: None,
            voice_receiver_telemetry: None,
            voice_telemetry_print_at: None,
            voice_render_fps: HashMap::new(),
            show_member_panel: None, // None = видима по умолчанию
        }
    }
}

impl fmt::Debug for MainState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MainState")
            .field("voice_video_textures_len", &self.voice_video_textures.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct State {
    pub(crate) screen: Screen,
    pub(crate) auth: AuthState,
    pub(crate) main: MainState,
    pub(crate) access_token: Option<String>,
    pub(crate) dark_mode: bool,
    pub(crate) user_id: Option<i64>,
    pub(crate) settings: Settings,
}

// ─── Auth screen ─────────────────────────────────────────────────────────────

pub(crate) fn auth_screen(ctx: &egui::Context, state: &mut State, api: &ApiClient) {
    let mut error = None;

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading(egui::RichText::new("Astrix").size(32.0));
            ui.add_space(24.0);

            ui.label(if state.auth.is_register { "Регистрация" } else { "Вход" });
            ui.add_space(8.0);

            ui.add(
                egui::TextEdit::singleline(&mut state.auth.username)
                    .hint_text("Имя пользователя")
                    .desired_width(240.0),
            );
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut state.auth.password)
                    .password(true)
                    .hint_text("Пароль")
                    .desired_width(240.0),
            );
            ui.add_space(8.0);

            // Remember me
            ui.checkbox(&mut state.auth.remember_me, "Запомнить данные входа");
            ui.add_space(8.0);

            if ui.button(if state.auth.is_register { "Зарегистрироваться" } else { "Войти" }).clicked() {
                let username = state.auth.username.clone();
                let password = state.auth.password.clone();
                let is_register = state.auth.is_register;
                let api = api.clone();

                let result = block_on(async move {
                    if is_register {
                        let req = RegisterRequest { username: username.clone(), password: password.clone(), public_e2ee_key: None };
                        api.register(&req).await?;
                        let login_req = LoginRequest { username, password };
                        api.login(&login_req).await
                    } else {
                        api.login(&LoginRequest { username, password }).await
                    }
                });

                match result {
                    Ok(tokens) => {
                        // Persist credentials if requested
                        if state.auth.remember_me {
                            state.settings.remember_me = true;
                            state.settings.saved_username = state.auth.username.clone();
                            state.settings.saved_password = state.auth.password.clone();
                        } else {
                            state.settings.remember_me = false;
                            state.settings.saved_username.clear();
                            state.settings.saved_password.clear();
                        }
                        state.settings.save();

                        let uid_key = tokens.user_id.to_string();
                        let last_server = state.settings.last_server.get(&uid_key).copied();

                        state.access_token = Some(tokens.access_token);
                        state.user_id = Some(tokens.user_id);
                        state.main.my_display_name = tokens.username.clone();
                        state.auth.error = None;
                        state.screen = Screen::Main;

                        // Restore last selected server
                        if let Some(sid) = last_server {
                            state.main.selected_server = Some(sid);
                        }
                    }
                    Err(e) => {
                        error = Some(format!("Ошибка авторизации: {e}"));
                    }
                }
                ctx.request_repaint();
            }

            ui.add_space(4.0);
            if ui.button(if state.auth.is_register { "У меня уже есть аккаунт" } else { "Создать аккаунт" }).clicked() {
                state.auth.is_register = !state.auth.is_register;
                state.auth.error = None;
            }

            if let Some(err) = &state.auth.error {
                ui.colored_label(egui::Color32::RED, err);
            }
        });
    });

    state.auth.error = error;
}

// ─── Constants ────────────────────────────────────────────────────────────────

const COL_SERVERS_W: f32 = 72.0;
const COL_CHANNELS_W: f32 = 240.0;
const COL_MEMBERS_W: f32 = 220.0;
const SERVER_CIRCLE_RADIUS: f32 = 24.0;
const AVATAR_RADIUS: f32 = 16.0;

// ─── Helper: circle with letter ──────────────────────────────────────────────

fn letter_circle(
    ui: &mut egui::Ui,
    letter: &str,
    radius: f32,
    selected: bool,
    tooltip: &str,
) -> egui::Response {
    let size = egui::vec2(radius * 2.0, radius * 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let resp = if tooltip.is_empty() { resp } else { resp.on_hover_text(tooltip) };
    let fill = if selected {
        ui.visuals().selection.bg_fill
    } else if resp.hovered() {
        ui.visuals().widgets.hovered.bg_fill
    } else {
        egui::Color32::from_gray(60)
    };
    let stroke = ui.visuals().widgets.inactive.bg_stroke;
    ui.painter().circle_filled(rect.center(), radius, fill);
    ui.painter().circle_stroke(rect.center(), radius, stroke);
    let font_size = (radius * 0.85).max(10.0);
    let galley = ui.painter().layout(
        letter.to_string(),
        egui::FontId::proportional(font_size),
        egui::Color32::WHITE,
        f32::INFINITY,
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, egui::Color32::WHITE);
    resp
}

/// Draw an avatar circle (image when available, else letter).
/// `speaking` — when true, draws a green ring around the circle.
fn avatar_circle(
    ui: &mut egui::Ui,
    display_name: &str,
    radius: f32,
    speaking: bool,
    texture: Option<&egui::TextureHandle>,
) {
    let ring_margin = if speaking { 3.0_f32 } else { 0.0 };
    let size = egui::vec2((radius + ring_margin) * 2.0, (radius + ring_margin) * 2.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let circle_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(radius * 2.0, radius * 2.0));
    if let Some(tex) = texture {
        let img = egui::Image::new(tex).fit_to_exact_size(circle_rect.size());
        img.paint_at(ui, circle_rect);
    } else {
        let letter = display_name.chars().next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());
        ui.painter().circle_filled(rect.center(), radius, egui::Color32::from_rgb(80, 100, 160));
        let font_size = (radius * 0.85).max(9.0);
        let galley = ui.painter().layout(
            letter,
            egui::FontId::proportional(font_size),
            egui::Color32::WHITE,
            f32::INFINITY,
        );
        let pos = rect.center() - galley.size() / 2.0;
        ui.painter().galley(pos, galley, egui::Color32::WHITE);
    }
    if speaking {
        ui.painter().circle_stroke(
            rect.center(),
            radius + 1.5,
            egui::Stroke::new(2.5, egui::Color32::from_rgb(67, 181, 129)),
        );
    }
}

/// Выполняет подключение к голосовому каналу (API + запуск движка). Вызывается из обработчика channel_panel.
fn apply_voice_join(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    video_frames: &mut Option<VideoFrames>,
    channel_id: i64,
    server_id: i64,
    user_id: Option<i64>,
) {
    if let Some(ref token) = state.access_token {
        let token = token.clone();
        match block_on(api.voice_join(&token, channel_id, server_id)) {
            Ok(resp) => {
                state.main.voice.channel_id = Some(channel_id);
                state.main.voice.server_id = Some(server_id);
                state.main.voice.participants = resp.participants.clone();
                state.main.channel_voice.insert(channel_id, resp.participants.clone());
                for p in &resp.participants {
                    let vol = state.settings.voice_volume_by_user
                        .get(&p.user_id.to_string())
                        .copied()
                        .unwrap_or(1.0);
                    state.main.voice.local_volumes.insert(p.user_id, vol);
                }
                state.main.voice.mic_muted = false;
                if let Some(old_tx) = engine_tx.take() {
                    old_tx.send(VoiceCmd::Stop).ok();
                }
                // Priority: external env var > UI settings > default "mft".
                // External env var is read here (before we overwrite it below).
                // This allows `$env:ASTRIX_DECODE_PATH=mft cargo run` to override
                // any saved setting, and running the exe directly uses the saved
                // setting (or the "mft" default if none is saved).
                let env_override = std::env::var("ASTRIX_DECODE_PATH").unwrap_or_default();
                let decode_path_owned: String = if env_override == "mft" || env_override == "cpu" {
                    env_override
                } else if state.settings.decode_path == "cpu" || state.settings.decode_path == "mft" {
                    state.settings.decode_path.clone()
                } else {
                    "mft".to_string()
                };
                let decode_path = decode_path_owned.as_str();
                // Phase 1.8: initialize shared D3D11 device for hardware MFT decode.
                // Must run before Room::connect() so the C++ VideoDecoderFactory gets
                // the ID3D11Device* via webrtc_mft_set_d3d11_device() before creating
                // any MftH264DecoderImpl instances.
                // Note: set_var below does NOT affect C++'s std::getenv on Windows
                // (SetEnvironmentVariableW ≠ CRT env table). The C++ default is already
                // MFT; set_var is kept only to reflect the chosen path in child processes.
                #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
                if decode_path == "mft" {
                    crate::mft_device::init_for_mft_decode();
                }
                std::env::set_var("ASTRIX_DECODE_PATH", decode_path);
                let rt = tokio::runtime::Handle::current();
                let (tx, vf) = spawn_voice_engine(rt);
                *video_frames = Some(vf);
                let receiver_telemetry = Arc::new(PipelineTelemetry::new());
                state.main.voice_receiver_telemetry = Some(Arc::clone(&receiver_telemetry));
                let session_stats = state.main.voice_stats
                    .get_or_insert_with(|| Arc::new(Mutex::new(VoiceSessionStats::default())))
                    .clone();
                tx.send(VoiceCmd::Start {
                    livekit_url: resp.livekit_url.clone(),
                    livekit_token: resp.token.clone(),
                    channel_id,
                    server_id,
                    api_base: api.base.clone(),
                    my_user_id: user_id.unwrap_or(0),
                    speaking: Arc::clone(&state.main.voice.speaking),
                    session_stats,
                    receiver_telemetry: Some(receiver_telemetry),
                }).ok();
                tx.send(VoiceCmd::SetInputVolume(state.main.voice.input_volume)).ok();
                tx.send(VoiceCmd::SetOutputVolume(state.main.voice.output_volume)).ok();
                *engine_tx = Some(tx);
            }
            Err(e) => eprintln!("voice join error: {e}"),
        }
        ctx.request_repaint();
    }
}

/// Отключение от голосового канала и остановка движка.
fn apply_voice_leave(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    video_frames: &mut Option<VideoFrames>,
) {
    if let (Some(ch_id), Some(ref token)) = (state.main.voice.channel_id, &state.access_token) {
        let token = token.clone();
        let _ = block_on(api.voice_leave(&token, ch_id));
    }
    if let Some(tx) = engine_tx.take() {
        tx.send(VoiceCmd::Stop).ok();
    }
    state.main.voice = VoiceState::default();
    state.main.voice_video_textures.clear();
    state.main.voice_render_fps.clear();
    state.main.voice_receiver_telemetry = None;
    // Phase 3.5: schedule texture deletion (freed via tex_manager in next update()).
    for (_, (egui_tex_id, _, _, _)) in state.main.voice_video_gpu_textures.drain() {
        state.main.voice_video_gpu_tex_pending_delete.push(egui_tex_id);
    }
    state.main.fullscreen_stream_user = None;
    state.main.stream_ended_prev_frame.clear();
    ctx.request_repaint();
}

// ─── Main screen ──────────────────────────────────────────────────────────────

/// Returns `true` if logout was requested.
pub(crate) fn main_screen(
    ctx: &egui::Context,
    state: &mut State,
    theme: &Theme,
    api: &ApiClient,
    media_textures: &HashMap<i64, egui::TextureHandle>,
    media_bytes: &HashMap<i64, (Vec<u8>, String)>,
    avatar_textures: &HashMap<i64, egui::TextureHandle>,
    engine_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    video_frames: &mut Option<VideoFrames>,
    // Phase 3.5: OpenGL context for GPU texture management (WGL_NV_DX_interop2).
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    gl_ctx: Option<std::sync::Arc<eframe::glow::Context>>,
    // Phase 3.5: WGL_NV_DX_interop2 manager (None = CPU path fallback).
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    mut gl_interop: Option<&mut crate::d3d11_gl_interop::D3d11GlInterop>,
    // Phase 3.5: eframe Frame for register_native_glow_texture().
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    eframe_frame: &mut eframe::Frame,
) -> bool {
    // Загрузка серверов/каналов/сообщений — в process_background_loads (app.rs), без block_on.

    // ── Delete/Leave server ───────────────────────────────────────────────
    if let Some(del_id) = state.main.server_to_delete.take() {
        if let Some(ref token) = state.access_token {
            let token = token.clone();
            let _ = block_on(api.delete_server(&token, del_id));
            state.main.servers.retain(|s| s.id != del_id);
            if state.main.selected_server == Some(del_id) {
                state.main.selected_server = None;
                state.main.channels.clear();
                state.main.server_members.clear();
                state.main.channel_voice.clear();
                state.main.channels_load_for = None;
                state.main.channels_load = LoadState::Idle;
                state.main.selected_channel = None;
                state.main.messages_load_for = None;
                state.main.messages_load = LoadState::Idle;
            }
            ctx.request_repaint();
        }
    }

    // Загрузка каналов и сообщений — в process_background_loads.

    // ── Обновление видеотекстур из VideoFrames (камера + стрим) ─────────────
    // Каждый кадр читаем новые декодированные фреймы из voice_video_frames и
    // загружаем/обновляем соответствующие TextureHandle в voice_video_textures.

    // Phase 3.5: Free registered GPU textures (e.g. after voice leave).
    // eframe's painter owns the GL texture via register_native_glow_texture(),
    // so freeing through tex_manager handles GL deletion automatically.
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    if !state.main.voice_video_gpu_tex_pending_delete.is_empty() {
        let tm = ctx.tex_manager();
        let mut mgr = tm.write();
        for tex_id in state.main.voice_video_gpu_tex_pending_delete.drain(..) {
            mgr.free(tex_id);
        }
    }

    // Phase 3.5: When stream ends, clear fullscreen overlay and remove stale textures.
    // Use only voice.participants (channel_voice can have stale streaming=true when WS payload lacks channel_id).
    // Debounce: remove texture only after 2 consecutive frames of non-streaming to avoid flickering.
    {
        let streaming_user_ids: HashSet<i64> = state.main.voice.participants.iter()
            .filter(|p| p.streaming)
            .map(|p| p.user_id)
            .collect();

        if let Some(uid) = state.main.fullscreen_stream_user {
            if !streaming_user_ids.contains(&uid) {
                state.main.fullscreen_stream_user = None;
            }
        }

        let stream_keys_non_streaming: Vec<i64> = state.main.voice_video_gpu_textures.keys()
            .chain(state.main.voice_video_textures.keys())
            .filter(|&&k| k < 0)
            .filter(|&&k| !streaming_user_ids.contains(&(-k - 1)))
            .copied()
            .collect::<std::collections::HashSet<_>>().into_iter().collect();

        // Debounce: only remove if we saw this key as non-streaming in the previous frame too.
        let stream_keys_to_remove: Vec<i64> = stream_keys_non_streaming.iter()
            .filter(|k| state.main.stream_ended_prev_frame.contains(k))
            .copied()
            .collect();

        state.main.stream_ended_prev_frame = stream_keys_non_streaming.into_iter().collect();

        if !stream_keys_to_remove.is_empty() {
            for key in &stream_keys_to_remove {
                state.main.stream_ended_prev_frame.remove(key);
                if let Some((egui_tex_id, _, _, _)) = state.main.voice_video_gpu_textures.remove(key) {
                    state.main.voice_video_gpu_tex_pending_delete.push(egui_tex_id);
                }
                state.main.voice_video_textures.remove(key);
                state.main.voice_render_fps.remove(key);
                let uid = -key - 1;
                if state.main.fullscreen_stream_user == Some(uid) {
                    state.main.fullscreen_stream_user = None;
                }
            }
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            if let Some(interop) = gl_interop.as_deref_mut() {
                interop.remove_keys(&stream_keys_to_remove);
            }
        }
    }

    if let Some(vf) = video_frames.as_ref() {
        let gui_draw_start = std::time::Instant::now();
        let frames_snap: Vec<(i64, crate::voice::VideoFrame)> = {
            let mut map = vf.lock();
            map.drain().collect()
        };
        let mut any_frame_processed = false;
        for (key, frame) in frames_snap {
            if frame.width == 0 || frame.height == 0 {
                continue;
            }
            any_frame_processed = true;
            let render_start = std::time::Instant::now();

            // Phase 3.5: GPU zero-copy path via WGL_NV_DX_interop2.
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            if let Some(handle) = frame.shared_handle {
                if let (Some(gl), Some(interop)) = (gl_ctx.as_ref(), gl_interop.as_deref_mut()) {
                    use eframe::glow::HasContext;
                    // Get or create a GL texture for this stream key.
                    // Tuple: (egui TextureId registered with painter, raw GL name for WGL)
                    let tex_pair = if let Some(&(eid, gid, _, _)) = state.main.voice_video_gpu_textures.get(&key) {
                        Some((eid, gid))
                    } else {
                        // Allocate a new GL texture name, set filter/wrap, register with eframe.
                        match unsafe { gl.create_texture() } {
                            Ok(tex) => {
                                let raw_id = tex.0.get();
                                unsafe {
                                    gl.bind_texture(eframe::glow::TEXTURE_2D, Some(tex));
                                    gl.tex_parameter_i32(eframe::glow::TEXTURE_2D, eframe::glow::TEXTURE_MIN_FILTER, eframe::glow::LINEAR as i32);
                                    gl.tex_parameter_i32(eframe::glow::TEXTURE_2D, eframe::glow::TEXTURE_MAG_FILTER, eframe::glow::LINEAR as i32);
                                    gl.tex_parameter_i32(eframe::glow::TEXTURE_2D, eframe::glow::TEXTURE_WRAP_S, eframe::glow::CLAMP_TO_EDGE as i32);
                                    gl.tex_parameter_i32(eframe::glow::TEXTURE_2D, eframe::glow::TEXTURE_WRAP_T, eframe::glow::CLAMP_TO_EDGE as i32);
                                    gl.bind_texture(eframe::glow::TEXTURE_2D, None);
                                }
                                // Register with eframe's glow painter so egui knows this texture.
                                let egui_tex_id = eframe_frame.register_native_glow_texture(tex);
                                Some((egui_tex_id, raw_id))
                            }
                            Err(e) => {
                                eprintln!("[Phase 3.5] gl.create_texture failed: {e}");
                                None
                            }
                        }
                    };
                    if let Some((egui_tex_id, gl_tex_id)) = tex_pair {
                        // Register (or re-register on handle change) the D3D11 shared texture.
                        match interop.update_texture(key, handle, gl_tex_id, frame.width, frame.height) {
                            Ok(()) => {
                                state.main.voice_video_gpu_textures.insert(
                                    key,
                                    (egui_tex_id, gl_tex_id, frame.width, frame.height),
                                );
                                state.main.voice_render_fps.entry(key).or_default().record();
                                if let Some(ref tel) = state.main.voice_receiver_telemetry {
                                    // Use max(1) so sub-microsecond ops show "0.00 ms" instead of "—".
                                    tel.set_render(render_start.elapsed().as_micros().max(1) as u64);
                                }
                                continue; // skip CPU path below
                            }
                            Err(e) => {
                                eprintln!("[Phase 3.5] WGL interop update_texture key={key}: {e}, falling back to CPU frame");
                            }
                        }
                    } else {
                        // GL texture creation failed — fall through to CPU path
                    }
                }
            }

            // CPU path: egui::ColorImage (fallback or when GL interop unavailable).
            if frame.rgba.is_empty() {
                // GPU frame where GL interop was unavailable — texture already uploaded
                // via shared_handle or frame was skipped. Record elapsed so render shows
                // "0.00 ms" instead of "—".
                if let Some(ref tel) = state.main.voice_receiver_telemetry {
                    tel.set_render(render_start.elapsed().as_micros().max(1) as u64);
                }
                continue;
            }
            let size = [frame.width as usize, frame.height as usize];
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba);
            let name = format!("voice_video_{}", key);
            match state.main.voice_video_textures.get_mut(&key) {
                Some(handle) => {
                    handle.set(color_image, egui::TextureOptions::LINEAR);
                }
                None => {
                    let handle = ctx.load_texture(&name, color_image, egui::TextureOptions::LINEAR);
                    state.main.voice_video_textures.insert(key, handle);
                }
            }
            state.main.voice_render_fps.entry(key).or_default().record();
            if let Some(ref tel) = state.main.voice_receiver_telemetry {
                tel.set_render(render_start.elapsed().as_micros().max(1) as u64);
            }
        }
        if let Some(ref tel) = state.main.voice_receiver_telemetry {
            // Only update gui_draw when there are actual frames to avoid storing 0 (which shows as —).
            if any_frame_processed {
                tel.set_gui_draw(gui_draw_start.elapsed().as_micros() as u64);
            }
            // Print from UI so gui_draw is fresh. Only print when frames are actively
            // being received so the log starts with the stream, not on idle voice chat.
            if any_frame_processed {
                let should_print = state.main.voice_telemetry_print_at
                    .map(|t| t.elapsed() >= Duration::from_secs(1))
                    .unwrap_or(true);
                if should_print {
                    tel.print("receiver");
                    state.main.voice_telemetry_print_at = Some(Instant::now());
                }
            }
        }
        // Repaint strategy: immediate when new frames were just processed (minimizes
        // latency), short delay otherwise (polls for new frames without burning 100% CPU
        // when vsync is disabled).
        if any_frame_processed {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(2));
        }
    }

    let server_selected = state.main.selected_server.is_some();
    let channel_selected = state.main.selected_channel.is_some();
    let is_text_channel = state.main.channels.iter()
        .find(|c| Some(c.id) == state.main.selected_channel)
        .map(|c| c.r#type == "text")
        .unwrap_or(false);

    // ── Dialog: voice server switch confirmation ──────────────────────────
    if let Some((target_ch, target_srv)) = state.main.voice_switch_confirm {
        let mut do_switch = false;
        let mut do_cancel = false;
        let cur_ch_name: String = state.main.channels.iter()
            .find(|c| state.main.voice.channel_id == Some(c.id))
            .map(|c| c.name.clone()).unwrap_or_default();
        let cur_srv_name: String = state.main.servers.iter()
            .find(|s| state.main.voice.server_id == Some(s.id))
            .map(|s| s.name.clone()).unwrap_or_default();
        egui::Window::new("Переключить голосовой канал?")
            .collapsible(false).resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!(
                    "Вы уже в голосовом канале «{}» на сервере «{}».",
                    cur_ch_name, cur_srv_name,
                ));
                ui.add_space(6.0);
                ui.label("Переключиться на новый канал?");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Переключить").clicked() { do_switch = true; }
                    if ui.button("Отмена").clicked()      { do_cancel = true; }
                });
            });
        if do_switch {
            // Leave current, then join target
            if let Some(cur_ch) = state.main.voice.channel_id {
                if let Some(ref token) = state.access_token {
                    let token = token.clone();
                    let _ = block_on(api.voice_leave(&token, cur_ch));
                }
            }
            // Stop current engine
            if let Some(tx) = engine_tx.take() {
                tx.send(VoiceCmd::Stop).ok();
            }
            state.main.voice = VoiceState::default();
            state.main.voice_video_textures.clear();
            state.main.voice_render_fps.clear();
            state.main.voice_receiver_telemetry = None;
            state.main.fullscreen_stream_user = None;
            state.main.stream_ended_prev_frame.clear();
            state.main.voice_switch_confirm = None;
            if let Some(ref token) = state.access_token {
                let token = token.clone();
                match block_on(api.voice_join(&token, target_ch, target_srv)) {
                    Ok(resp) => {
                        state.main.voice.channel_id = Some(target_ch);
                        state.main.voice.server_id = Some(target_srv);
                        state.main.voice.participants = resp.participants.clone();
                        state.main.channel_voice.insert(target_ch, resp.participants);
                        // Start new engine for the switched channel (LiveKit)
                        let rt = tokio::runtime::Handle::current();
                        let (tx, vf) = spawn_voice_engine(rt);
                        *video_frames = Some(vf);
                        let receiver_telemetry = Arc::new(PipelineTelemetry::new());
                        state.main.voice_receiver_telemetry = Some(Arc::clone(&receiver_telemetry));
                        let session_stats = state.main.voice_stats
                            .get_or_insert_with(|| Arc::new(Mutex::new(VoiceSessionStats::default())))
                            .clone();
                        tx.send(VoiceCmd::Start {
                            livekit_url: resp.livekit_url.clone(),
                            livekit_token: resp.token.clone(),
                            channel_id: target_ch,
                            server_id: target_srv,
                            api_base: api.base.clone(),
                            my_user_id: state.user_id.unwrap_or(0),
                            speaking: Arc::clone(&state.main.voice.speaking),
                            session_stats,
                            receiver_telemetry: Some(receiver_telemetry),
                        }).ok();
                            tx.send(VoiceCmd::SetInputVolume(state.main.voice.input_volume)).ok();
                            tx.send(VoiceCmd::SetOutputVolume(state.main.voice.output_volume)).ok();
                            *engine_tx = Some(tx);
                    }
                    Err(e) => eprintln!("voice join error: {e}"),
                }
            }
            ctx.request_repaint();
        }
        if do_cancel {
            state.main.voice_switch_confirm = None;
        }
    }

    // ── Dialog: create server ─────────────────────────────────────────────
    if state.main.show_create_server_dialog {
        let mut should_create = false;
        let mut should_cancel = false;
        egui::Window::new("Создать сервер")
            .collapsible(false).resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Название сервера:");
                ui.add_space(4.0);
                ui.add(egui::TextEdit::singleline(&mut state.main.new_server_name)
                    .hint_text("Мой сервер").desired_width(220.0));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Создать").clicked() { should_create = true; }
                    if ui.button("Отмена").clicked()  { should_cancel = true; }
                });
            });
        if should_create {
            let name = state.main.new_server_name.trim().to_string();
            if !name.is_empty() {
                if let Some(ref token) = state.access_token {
                    let token = token.clone();
                    if let Ok(server) = block_on(api.create_server(&token, &name)) {
                        let new_id = server.id;
                        state.main.servers.push(server);
                        state.main.new_server_name.clear();
                        state.main.show_create_server_dialog = false;
                        state.main.selected_server = Some(new_id);
                    }
                    ctx.request_repaint();
                }
            }
        }
        if should_cancel {
            state.main.show_create_server_dialog = false;
            state.main.new_server_name.clear();
        }
    }

    // ── Dialog: create channel ────────────────────────────────────────────
    if state.main.show_create_channel_dialog {
        let mut should_create = false;
        let mut should_cancel = false;
        egui::Window::new("Создать канал")
            .collapsible(false).resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Название канала:");
                ui.add_space(4.0);
                ui.add(egui::TextEdit::singleline(&mut state.main.new_channel_name)
                    .hint_text("общий").desired_width(220.0));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.radio_value(&mut state.main.new_channel_is_voice, false, "Текстовый");
                    ui.radio_value(&mut state.main.new_channel_is_voice, true,  "Голосовой");
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Создать").clicked() { should_create = true; }
                    if ui.button("Отмена").clicked()  { should_cancel = true; }
                });
            });
        if should_create {
            let name = state.main.new_channel_name.trim().to_string();
            let ch_type = if state.main.new_channel_is_voice { "voice" } else { "text" };
            if !name.is_empty() {
                if let (Some(server_id), Some(ref token)) =
                    (state.main.selected_server, &state.access_token)
                {
                    let token = token.clone();
                    if let Ok(ch) = block_on(api.create_channel(&token, server_id, &name, ch_type)) {
                        // WS broadcast will arrive; add locally too for instant feedback
                        if !state.main.channels.iter().any(|c| c.id == ch.id) {
                            state.main.channels.push(ch);
                        }
                        state.main.new_channel_name.clear();
                        state.main.show_create_channel_dialog = false;
                    }
                    ctx.request_repaint();
                }
            }
        }
        if should_cancel {
            state.main.show_create_channel_dialog = false;
            state.main.new_channel_name.clear();
        }
    }

    // ── Dialog: invite user ───────────────────────────────────────────────
    if state.main.show_invite_dialog {
        let mut should_invite = false;
        let mut should_close = false;
        let srv_label: String = state.main.servers.iter()
            .find(|s| Some(s.id) == state.main.selected_server)
            .map(|s| s.name.clone()).unwrap_or_default();
        egui::Window::new(format!("Пригласить на «{}»", srv_label))
            .collapsible(false).resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                if let Some(uid) = state.user_id {
                    ui.label(egui::RichText::new("Ваш ID").weak());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(uid.to_string()).monospace().strong());
                        if ui.small_button("📋").on_hover_text("Копировать").clicked() {
                            ctx.output_mut(|o| o.copied_text = uid.to_string());
                        }
                    });
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);
                }
                ui.label("ID пользователя для приглашения:");
                ui.add_space(4.0);
                ui.add(egui::TextEdit::singleline(&mut state.main.invite_user_id_input)
                    .hint_text("Числовой ID").desired_width(220.0));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Пригласить").clicked() { should_invite = true; }
                    if ui.button("Закрыть").clicked()    { should_close  = true; }
                });
                if let Some(ref msg) = state.main.invite_msg {
                    ui.add_space(4.0);
                    ui.label(msg);
                }
            });
        if should_invite {
            let id_str = state.main.invite_user_id_input.trim().to_string();
            if let Ok(invite_uid) = id_str.parse::<i64>() {
                if let (Some(server_id), Some(ref token)) =
                    (state.main.selected_server, &state.access_token)
                {
                    let token = token.clone();
                    match block_on(api.add_member(&token, server_id, invite_uid)) {
                        Ok(_) => {
                            state.main.invite_msg = Some("Пользователь добавлен!".to_string());
                            state.main.invite_user_id_input.clear();
                        }
                        Err(_) => {
                            state.main.invite_msg = Some("Ошибка: не найден или уже в сервере.".to_string());
                        }
                    }
                    ctx.request_repaint();
                }
            } else {
                state.main.invite_msg = Some("Введите корректный числовой ID.".to_string());
            }
        }
        if should_close {
            state.main.show_invite_dialog = false;
            state.main.invite_user_id_input.clear();
            state.main.invite_msg = None;
        }
    }

    // ── Dialog: user settings ─────────────────────────────────────────────
    if state.main.show_settings_dialog {
        let mut should_close = false;
        let mut do_nick = false;
        let mut do_avatar = false;
        let mut new_input_vol:  Option<f32> = None;
        let mut new_output_vol: Option<f32> = None;
        egui::Window::new("Настройки")
            .collapsible(false).resizable(false)
            .min_width(260.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.heading("Профиль");
                ui.add_space(6.0);
                ui.label("Никнейм на сервере:");
                ui.add(egui::TextEdit::singleline(&mut state.main.settings_nickname_input)
                    .hint_text("Ваш ник").desired_width(220.0));
                if ui.button("Сохранить никнейм").clicked() {
                    do_nick = true;
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label("Аватарка:");
                if ui.button("Выбрать файл...").clicked() {
                    do_avatar = true;
                }
                if let Some(ref p) = state.main.settings_avatar_path {
                    ui.label(egui::RichText::new(p.display().to_string()).small().weak());
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                ui.heading("Голос");
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Громкость микрофона:").small());
                let mut iv = state.main.voice.input_volume;
                let slider_in = egui::Slider::new(&mut iv, 0.0_f32..=2.0_f32)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .show_value(true);
                if ui.add(slider_in).changed() {
                    new_input_vol = Some(iv);
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Громкость динамиков:").small());
                let mut ov = state.main.voice.output_volume;
                let slider_out = egui::Slider::new(&mut ov, 0.0_f32..=4.0_f32)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .show_value(true);
                if ui.add(slider_out).changed() {
                    new_output_vol = Some(ov);
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                ui.heading("Видео");
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Декодирование входящего видео:").small());
                let mut dp: String = state.settings.decode_path.clone();
                egui::ComboBox::from_id_source("decode_path")
                    .selected_text(if dp == "mft" { "MFT (Media Foundation)" } else { "CPU (OpenH264)" })
                    .show_ui(ui, |ui| {
                        let _ = ui.selectable_value(&mut dp, "cpu".to_string(), "CPU (OpenH264)");
                        let _ = ui.selectable_value(&mut dp, "mft".to_string(), "MFT (Media Foundation)");
                    });
                if dp != state.settings.decode_path {
                    state.settings.decode_path = dp;
                }
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Гамма декодера (MFT, GPU): pow(rgb, 1/γ). 0 = выкл.").small());
                let mut gamma = state.settings.video_decoder_gamma;
                if ui.add(egui::Slider::new(&mut gamma, 0.0..=3.0).step_by(0.01).suffix("")).changed() {
                    state.settings.video_decoder_gamma = gamma;
                    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
                    crate::d3d11_rgba::set_video_decoder_gamma(gamma);
                }
                ui.add_space(8.0);
                if let Some(ref msg) = state.main.settings_msg {
                    ui.label(msg);
                }
                if ui.button("Закрыть").clicked() { should_close = true; }
            });

        if let Some(v) = new_input_vol {
            state.main.voice.input_volume = v;
            if let Some(tx) = engine_tx.as_ref() {
                tx.send(VoiceCmd::SetInputVolume(v)).ok();
            }
        }
        if let Some(v) = new_output_vol {
            state.main.voice.output_volume = v;
            if let Some(tx) = engine_tx.as_ref() {
                tx.send(VoiceCmd::SetOutputVolume(v)).ok();
            }
        }

        if do_nick {
            let nick = state.main.settings_nickname_input.trim().to_string();
            if let (Some(server_id), Some(ref token)) =
                (state.main.selected_server, &state.access_token)
            {
                let token = token.clone();
                match block_on(api.set_nickname(&token, server_id, &nick)) {
                    Ok(_) => {
                        state.main.settings_msg = Some("Никнейм обновлён.".to_string());
                        state.main.my_display_name = if nick.is_empty() {
                            state.auth.username.clone()
                        } else {
                            nick
                        };
                    }
                    Err(e) => { state.main.settings_msg = Some(format!("Ошибка: {e}")); }
                }
                ctx.request_repaint();
            }
        }

        if do_avatar {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Изображения", &["png", "jpg", "jpeg", "gif", "webp"])
                .pick_file()
            {
                state.main.settings_avatar_path = Some(path.clone());
                // Upload immediately
                if let Some(ref token) = state.access_token {
                    if let Ok(bytes) = std::fs::read(&path) {
                        let mime = mime_from_path(&path);
                        let token = token.clone();
                        match block_on(api.set_avatar(&token, bytes, &mime)) {
                            Ok(_) => { state.main.settings_msg = Some("Аватарка обновлена.".to_string()); }
                            Err(e) => { state.main.settings_msg = Some(format!("Ошибка: {e}")); }
                        }
                        ctx.request_repaint();
                    }
                }
            }
        }

        if should_close {
            state.main.show_settings_dialog = false;
            state.main.settings_msg = None;
            state.settings.save();
        }
    }

    // ── Dialog: channel rename ────────────────────────────────────────────
    if let Some((ch_id, ref mut rename_input)) = state.main.channel_rename.clone() {
        let ch_name_orig = state.main.channels.iter()
            .find(|c| c.id == ch_id).map(|c| c.name.clone()).unwrap_or_default();
        let mut should_save = false;
        let mut should_cancel = false;
        let mut new_name = rename_input.clone();
        egui::Window::new(format!("Переименовать «{}»", ch_name_orig))
            .collapsible(false).resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut new_name)
                    .desired_width(220.0));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Сохранить").clicked() { should_save = true; }
                    if ui.button("Отмена").clicked()    { should_cancel = true; }
                });
            });
        if let Some(ref mut ri) = state.main.channel_rename.as_mut().map(|(_, ri)| ri) {
            **ri = new_name.clone();
        }
        if should_save {
            let name = new_name.trim().to_string();
            if !name.is_empty() {
                if let Some(ref token) = state.access_token {
                    let token = token.clone();
                    if let Ok(ch) = block_on(api.rename_channel(&token, ch_id, &name)) {
                        for c in &mut state.main.channels {
                            if c.id == ch.id { c.name = ch.name; break; }
                        }
                    }
                    ctx.request_repaint();
                }
            }
            state.main.channel_rename = None;
        }
        if should_cancel {
            state.main.channel_rename = None;
        }
    }

    // ── Unified horizontal line across all three blocks (channels, chat, members) ──
    egui::TopBottomPanel::top("unified_header_line")
        .exact_height(40.0)
        .show_separator_line(true)
        .show(ctx, |ui| {
            // Phase 3.5: sRGB diagnostic — log GL_FRAMEBUFFER_SRGB state when GPU video active.
            // If ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB=1, also disable it (test for double sRGB).
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            if !state.main.voice_video_gpu_textures.is_empty() {
                let disable_fb_srgb = std::env::var("ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB").as_deref() == Ok("1");
                static LOGGED_SRGB: AtomicBool = AtomicBool::new(false);
                let rect = ui.available_rect_before_wrap();
                let callback = egui::PaintCallback {
                    rect,
                    callback: Arc::new(egui_glow::CallbackFn::new(move |_info, painter| {
                        let gl = painter.gl();
                        unsafe {
                            use eframe::glow::HasContext;
                            let was_enabled = gl.is_enabled(eframe::glow::FRAMEBUFFER_SRGB);
                            if !LOGGED_SRGB.swap(true, Ordering::Relaxed) {
                                eprintln!(
                                    "[Phase 3.5] GL_FRAMEBUFFER_SRGB was {} (if washed out, try ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB=1)",
                                    if was_enabled { "enabled" } else { "disabled" }
                                );
                            }
                            if disable_fb_srgb {
                                gl.disable(eframe::glow::FRAMEBUFFER_SRGB);
                            }
                        }
                    })),
                };
                ui.painter().add(callback);
            }
            ui.add_space(0.0);
        });

    // ── Left panel: server icons (guild_panel) ──────────────────────────────
    egui::SidePanel::left("panel_servers")
        .exact_width(guild_panel::GUILD_PANEL_WIDTH)
        .resizable(false)
        .show(ctx, |ui| {
            let selected_server = state.main.selected_server;
            let dark_mode = state.dark_mode;
            let servers = state.main.servers.clone();
            let mut on_action = |act: guild_panel::GuildPanelAction| {
                match act {
                    guild_panel::GuildPanelAction::SelectDms => {
                        state.main.selected_server = None;
                    }
                    guild_panel::GuildPanelAction::SelectServer(id) => {
                        state.main.selected_server = Some(id);
                        if let Some(uid) = state.user_id {
                            state.settings.last_server.insert(uid.to_string(), id);
                            state.settings.save();
                        }
                    }
                    guild_panel::GuildPanelAction::AddServer => {
                        state.main.show_create_server_dialog = true;
                    }
                    guild_panel::GuildPanelAction::Explore => {
                        println!("Feature not implemented yet");
                    }
                    guild_panel::GuildPanelAction::DeleteServer(id) => {
                        state.main.server_to_delete = Some(id);
                    }
                    guild_panel::GuildPanelAction::ThemeToggle => {
                        state.dark_mode = !state.dark_mode;
                        state.settings.dark_mode = state.dark_mode;
                        state.settings.save();
                    }
                    guild_panel::GuildPanelAction::RetryServers => {
                        state.main.retry_servers = true;
                    }
                }
            };
            let servers_loading = state.main.servers_load == LoadState::Loading;
            let servers_error = match &state.main.servers_load {
                LoadState::Error(s) => Some(s.as_str()),
                _ => None,
            };
            guild_panel::show(
                ctx,
                ui,
                GuildPanelParams {
                    theme,
                    servers: servers.as_slice(),
                    selected_server,
                    on_action: &mut on_action,
                    dark_mode,
                    servers_loading,
                    servers_error,
                },
            );
        });

    // ── Left panel: channels list (channel_panel) ───────────────────────────
    let mut should_logout = false;
    if server_selected {
        let text_chs: Vec<(i64, String)> = state.main.channels
            .iter()
            .filter(|c| c.r#type == "text")
            .map(|c| (c.id, c.name.clone()))
            .collect();
        let voice_chs: Vec<(i64, String)> = state.main.channels
            .iter()
            .filter(|c| c.r#type == "voice")
            .map(|c| (c.id, c.name.clone()))
            .collect();
        let voice_snapshot = ChannelPanelVoiceSnapshot {
            channel_id: state.main.voice.channel_id,
            server_id: state.main.voice.server_id,
            mic_muted: state.main.voice.mic_muted,
            output_muted: state.main.voice.output_muted,
            channel_voice: state.main.channel_voice.clone(),
            speaking: state.main.voice.speaking.lock().clone(),
        };
        let server_name = state.main.servers
            .iter()
            .find(|s| Some(s.id) == state.main.selected_server)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let server_id = state.main.selected_server.unwrap_or(0);
        let user_display = if state.main.my_display_name.is_empty() {
            state.auth.username.clone()
        } else {
            state.main.my_display_name.clone()
        };
        let mut channel_actions: Vec<ChannelPanelAction> = Vec::new();
        egui::SidePanel::left("panel_channels")
            .exact_width(channel_panel::CHANNEL_PANEL_WIDTH)
            .resizable(false)
            .show(ctx, |ui| {
                let channels_load = match &state.main.channels_load {
                    LoadState::Idle => ChannelsLoadState::Idle,
                    LoadState::Loading => ChannelsLoadState::Loading,
                    LoadState::Loaded => ChannelsLoadState::Loaded,
                    LoadState::Error(s) => ChannelsLoadState::Error(s.clone()),
                };
                channel_panel::show(
                    ctx,
                    ui,
                    ChannelPanelParams {
                        theme,
                        server_name: &server_name,
                        server_id,
                        text_channels: &text_chs,
                        voice_channels: &voice_chs,
                        selected_channel_id: state.main.selected_channel,
                        voice: voice_snapshot.clone(),
                        user_display: &user_display,
                        user_id: state.user_id,
                        on_action: &mut |a| channel_actions.push(a.clone()),
                        avatar_texture: state.user_id.and_then(|id| avatar_textures.get(&id)),
                        channels_load,
                    },
                );
            });

        let user_id = state.user_id;
        for act in channel_actions {
            match act {
                ChannelPanelAction::SelectChannel(id) => {
                    state.main.selected_channel = Some(id);
                }
                ChannelPanelAction::JoinVoice { channel_id, server_id: srv_id } => {
                    if state.main.voice.channel_id == Some(channel_id) {
                        // already in this channel
                    } else if state.main.voice.channel_id.is_none() {
                        apply_voice_join(ctx, state, api, engine_tx, video_frames, channel_id, srv_id, user_id);
                    } else if state.main.voice.server_id == Some(srv_id) {
                        apply_voice_join(ctx, state, api, engine_tx, video_frames, channel_id, srv_id, user_id);
                    } else {
                        state.main.voice_switch_confirm = Some((channel_id, srv_id));
                    }
                }
                ChannelPanelAction::LeaveVoice => {
                    apply_voice_leave(ctx, state, api, engine_tx, video_frames);
                }
                ChannelPanelAction::SetMicMuted(muted) => {
                    state.main.voice.mic_muted = muted;
                    if let Some(tx) = engine_tx.as_ref() {
                        tx.send(VoiceCmd::SetMicMuted(muted)).ok();
                    }
                    if let (Some(ch_id), Some(ref token)) =
                        (state.main.voice.channel_id, &state.access_token)
                    {
                        let token = token.clone();
                        let _ = block_on(api.voice_update_state(
                            &token,
                            ch_id,
                            state.main.voice.mic_muted,
                            state.main.voice.camera_on,
                            state.main.voice.screen_on,
                        ));
                    }
                    ctx.request_repaint();
                }
                ChannelPanelAction::SetOutputMuted(muted) => {
                    state.main.voice.output_muted = muted;
                    if let Some(tx) = engine_tx.as_ref() {
                        tx.send(VoiceCmd::SetOutputMuted(muted)).ok();
                    }
                    ctx.request_repaint();
                }
                ChannelPanelAction::CreateChannel => {
                    state.main.show_create_channel_dialog = true;
                }
                ChannelPanelAction::Invite => {
                    state.main.show_invite_dialog = true;
                    state.main.invite_msg = None;
                }
                ChannelPanelAction::ChannelSettings(id, name) => {
                    state.main.channel_rename = Some((id, name));
                }
                ChannelPanelAction::OpenSettings => {
                    state.main.show_settings_dialog = true;
                    state.main.settings_nickname_input = state.main.my_display_name.clone();
                    state.main.settings_msg = None;
                }
                ChannelPanelAction::Logout => {
                    should_logout = true;
                }
                ChannelPanelAction::RetryChannels => {
                    state.main.retry_channels = true;
                }
            }
        }

        if state.main.voice_pending_leave {
            apply_voice_leave(ctx, state, api, engine_tx, video_frames);
        }
    }

    // ── Right panel: members (member_panel) ──────────────────────────────────
    let show_members = state.main.show_member_panel.unwrap_or(true);
    if server_selected && show_members {
        let online_count = state.main.online_users.len();
        let server_owner_id = state.main.servers.iter()
            .find(|s| Some(s.id) == state.main.selected_server)
            .map(|s| s.owner_id).unwrap_or(0);
        let mut members_snap: Vec<MemberSnapshot> = state.main.server_members.iter()
            .map(|m| {
                let online = state.main.online_users.contains(&m.user_id);
                let is_owner = m.is_owner || m.user_id == server_owner_id;
                let display = if m.display_name.is_empty() { m.username.clone() } else { m.display_name.clone() };
                MemberSnapshot {
                    user_id: m.user_id,
                    display_name: display,
                    username: m.username.clone(),
                    is_owner,
                    online,
                }
            })
            .collect();
        members_snap.sort_by(|a, b| b.online.cmp(&a.online)); // online first

        let speaking_snap = state.main.voice.speaking.lock().clone();

        egui::SidePanel::right("panel_members")
            .exact_width(member_panel::MEMBER_PANEL_WIDTH)
            .resizable(false)
            .show(ctx, |ui| {
                member_panel::show(ctx, ui, MemberPanelParams {
                    theme,
                    members: &members_snap,
                    online_count,
                    speaking: &speaking_snap,
                    avatar_textures,
                });
            });
    }

    // ── Central panel: chat ───────────────────────────────────────────────
    egui::CentralPanel::default().show(ctx, |ui| {
        if !channel_selected {
            ui.centered_and_justified(|ui| {
                ui.label(if server_selected {
                    "Выберите канал для начала общения"
                } else {
                    "Выберите сервер"
                });
            });
            return;
        }

        let ch_name: String = state.main.channels.iter()
            .find(|c| Some(c.id) == state.main.selected_channel)
            .map(|c| if c.r#type == "text" { format!("# {}", c.name) }
                     else { format!("\u{1F50A} {}", c.name) })
            .unwrap_or_default();

        let mut chat_actions: Vec<ChatPanelAction> = Vec::new();
        let mut pick_file = false;

        if is_text_channel {
            let typing_users: Vec<(i64, String)> = state.main.typing_users
                .iter()
                .map(|(id, name, _)| (*id, name.clone()))
                .collect();
            let messages_loading = state.main.messages_load == LoadState::Loading;
            let messages_load_error = match &state.main.messages_load {
                LoadState::Error(s) => Some(s.clone()),
                _ => None,
            };
            chat_panel::show(ctx, ui, ChatPanelParams {
                theme,
                channel_name: &ch_name,
                channel_description: None,
                messages: &state.main.messages,
                new_message: &mut state.main.new_message,
                typing_users: &typing_users,
                pending_attachment: state.main.pending_attachment.as_ref(),
                current_user_id: state.user_id,
                server_members: &state.main.server_members,
                media_textures,
                media_bytes,
                on_action: &mut |a| chat_actions.push(a),
                messages_load_error,
                messages_loading,
            });

            for act in chat_actions {
                match act {
                    ChatPanelAction::SendMessage => {
                        let text = state.main.new_message.trim().to_string();
                        let has_attachment = state.main.pending_attachment.is_some();
                        if !text.is_empty() || has_attachment {
                            if let (Some(ch_id), Some(ref token)) =
                                (state.main.selected_channel, &state.access_token)
                            {
                                let token = token.clone();
                                let attachments: Vec<AttachmentMeta> = state.main.pending_attachment
                                    .take().into_iter().collect();
                                let content = if text.is_empty() {
                                    attachments.first().map(|a| a.filename.clone()).unwrap_or_default()
                                } else {
                                    text
                                };
                                state.main.pending_attachment_bytes = None;
                                if let Ok(msg) = block_on(api.send_message(&token, ch_id, &content, attachments)) {
                                    if !state.main.messages.iter().any(|m| m.id == msg.id) {
                                        state.main.messages.push(msg);
                                    }
                                }
                                state.main.new_message.clear();
                                ctx.request_repaint();
                            }
                        }
                    }
                    ChatPanelAction::AttachRequest => pick_file = true,
                    ChatPanelAction::ClearAttachment => {
                        state.main.pending_attachment = None;
                        state.main.pending_attachment_bytes = None;
                    }
                    ChatPanelAction::Threads
                    | ChatPanelAction::Notifications
                    | ChatPanelAction::Pinned
                    | ChatPanelAction::Search
                    | ChatPanelAction::Inbox
                    | ChatPanelAction::Help => {
                        println!("Feature not implemented yet");
                    }
                    ChatPanelAction::ToggleMemberList => {
                        let current = state.main.show_member_panel.unwrap_or(true);
                        state.main.show_member_panel = Some(!current);
                        ctx.request_repaint();
                    }
                    ChatPanelAction::StubGif | ChatPanelAction::StubEmoji | ChatPanelAction::StubStickers => {
                        println!("Feature not implemented yet");
                    }
                    ChatPanelAction::RetryMessages => {
                        state.main.retry_messages = true;
                    }
                }
            }
        }

        // File picker (после отрисовки панели, чтобы не держать мутабельные заимствования)
        if pick_file {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                if let Ok(bytes) = std::fs::read(&path) {
                    let filename = path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string();
                    let mime = mime_from_path(&path);
                    if let (Some(server_id), Some(ref token)) =
                        (state.main.selected_server, &state.access_token)
                    {
                        let token = token.clone();
                        match block_on(api.upload_media(&token, server_id, &filename, &mime, bytes.clone())) {
                            Ok(resp) => {
                                let att = AttachmentMeta {
                                    media_id: resp.id,
                                    filename: resp.filename,
                                    mime_type: resp.mime_type,
                                    size_bytes: resp.size_bytes,
                                };
                                state.main.pending_attachment = Some(att);
                                state.main.pending_attachment_bytes = Some((bytes, mime));
                            }
                            Err(e) => eprintln!("Media upload error: {e}"),
                        }
                        ctx.request_repaint();
                    }
                }
            }
        }

        if !is_text_channel {
            // ── Voice grid UI (Stage 5): grid of participant tiles with avatar/video and speaking indicator
            let in_this_voice = state.main.voice.channel_id == state.main.selected_channel;
            let participants: Vec<VoiceParticipant> = if in_this_voice {
                state.main.voice.participants.clone()
            } else {
                state.main.selected_channel
                    .and_then(|ch_id| state.main.channel_voice.get(&ch_id).cloned())
                    .unwrap_or_default()
            };
            let speaking_snap: HashMap<i64, bool> = state.main.voice.speaking.lock().clone();

            if in_this_voice {
                ctx.request_repaint_after(std::time::Duration::from_millis(80));
            }
            if participants.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(if in_this_voice {
                        "В канале пока никого нет"
                    } else {
                        "Подключитесь к каналу, чтобы видеть участников"
                    });
                });
            } else {
                // Adaptive grid: 1 → ~full screen, 2 → 50/50, 4 → 2x2, N → auto.
                // Multiple video tracks per user (camera + screen): when voice_video_textures
                // is keyed by (user_id, track_id), show multiple rounded-rect tiles per user.
                let n = participants.len().max(1);
                let avail = ui.available_size_before_wrap();
                let (n_cols, tile_w, tile_h) = match n {
                    1 => (1, (avail.x * 0.95).min(400.0), (avail.y * 0.6).min(300.0)),
                    2 => (2, avail.x * 0.5 - 4.0, (avail.y * 0.5).min(280.0)),
                    3 | 4 => (2, avail.x * 0.5 - 4.0, (avail.y * 0.45).min(220.0)),
                    _ => {
                        let cols = (n as f32).sqrt().ceil() as usize;
                        let c = cols.max(1);
                        let w = (avail.x / c as f32) - 4.0;
                        let rows = (n + c - 1) / c;
                        let h = (avail.y / rows as f32).min(180.0);
                        (c, w, h)
                    }
                };
                const ROUNDING: f32 = 12.0; // Rounded rectangles for avatars/video

                egui::ScrollArea::vertical()
                    .id_source("voice_grid_scroll")
                    .show(ui, |ui| {
                        for chunk in participants.chunks(n_cols) {
                            ui.horizontal(|ui| {
                                for p in chunk {
                                    ui.push_id(p.user_id, |ui| {
                                        let (rect, resp) = ui.allocate_exact_size(
                                            egui::vec2(tile_w, tile_h),
                                            egui::Sense::hover(),
                                        );
                                        let is_speaking = *speaking_snap.get(&p.user_id).unwrap_or(&false);
                                        let is_locally_muted = in_this_voice
                                            && state.main.voice.locally_muted.contains(&p.user_id);

                                        // Background (rounded rect)
                                        let fill = ui.visuals().faint_bg_color;
                                        ui.painter().rect_filled(rect, egui::Rounding::same(ROUNDING), fill);

                                        // Avatar/video area: prefer stream texture when p.streaming
                                        let content_margin = 8.0;
                                        let avatar_rect = rect.shrink2(egui::vec2(content_margin, content_margin));
                                        let avatar_rect = egui::Rect::from_min_max(
                                            avatar_rect.min,
                                            egui::pos2(avatar_rect.max.x, avatar_rect.min.y + (avatar_rect.height() * 0.72)),
                                        );
                                        let stream_key = video_frame_key(p.user_id, true);
                                        let camera_key = p.user_id;
                                        // Always check stream_key first: video frames may arrive before
                                        // the server sets p.streaming = true (race condition), so texture
                                        // presence is the authoritative signal that a stream is active.
                                        let tex_key =
                                            state.main.voice_video_gpu_textures.get(&stream_key).map(|_| stream_key)
                                            .or_else(|| state.main.voice_video_textures.get(&stream_key).map(|_| stream_key))
                                            .or_else(|| state.main.voice_video_gpu_textures.get(&camera_key).map(|_| camera_key))
                                            .or_else(|| state.main.voice_video_textures.get(&camera_key).map(|_| camera_key));
                                        if let Some(key) = tex_key {
                                            // Phase 3.5: prefer GPU zero-copy texture (TextureId::User),
                                            // fall back to CPU-uploaded TextureHandle.
                                            let rendered = if let Some(&(egui_tex_id, _, w, h)) = state.main.voice_video_gpu_textures.get(&key) {
                                                let sized = egui::load::SizedTexture::new(
                                                    egui_tex_id,
                                                    egui::vec2(w as f32, h as f32),
                                                );
                                                ui.put(avatar_rect, egui::Image::new(sized).fit_to_exact_size(avatar_rect.size()));
                                                true
                                            } else {
                                                false
                                            };
                                            if !rendered {
                                            if let Some(tex) = state.main.voice_video_textures.get(&key) {
                                                let size = avatar_rect.size();
                                                ui.put(avatar_rect, egui::Image::new(tex).fit_to_exact_size(size));
                                            }}
                                                if is_speaking && !p.streaming {
                                                    ui.painter().rect_stroke(
                                                        avatar_rect.expand(2.0),
                                                        egui::Rounding::same(ROUNDING + 2.0),
                                                        egui::Stroke::new(2.0, egui::Color32::from_rgb(67, 181, 129)),
                                                    );
                                                }
                                                // Stream tile: overlay with fullscreen + mute in corner
                                                if p.streaming && in_this_voice {
                                                    let corner = avatar_rect.max - egui::vec2(4.0, 4.0);
                                                    let btn_size = egui::vec2(28.0, 28.0);
                                                    let fullscreen_rect = egui::Rect::from_min_size(corner - egui::vec2(btn_size.x * 2.0 + 4.0, 0.0), btn_size);
                                                    let mute_rect = egui::Rect::from_min_size(corner - egui::vec2(btn_size.x, 0.0), btn_size);
                                                    ui.allocate_ui_at_rect(fullscreen_rect, |ui| {
                                                        if ui.button("⛶").on_hover_text("На весь экран").clicked() {
                                                            state.main.fullscreen_stream_user = Some(p.user_id);
                                                        }
                                                    });
                                                    ui.allocate_ui_at_rect(mute_rect, |ui| {
                                                        let muted = state.main.voice.locally_muted.contains(&p.user_id);
                                                        let label = if muted { "🔊" } else { "🔇" };
                                                        if ui.button(label).on_hover_text(if muted { "Включить звук трансляции" } else { "Заглушить трансляцию" }).clicked() {
                                                            if muted {
                                                                state.main.voice.locally_muted.remove(&p.user_id);
                                                                let restore = state.main.voice.local_volumes.get(&p.user_id).copied().unwrap_or(1.0);
                                                                if let Some(tx) = engine_tx.as_ref() {
                                                                    tx.send(VoiceCmd::SetUserVolume(p.user_id, restore)).ok();
                                                                }
                                                            } else {
                                                                state.main.voice.locally_muted.insert(p.user_id);
                                                                if let Some(tx) = engine_tx.as_ref() {
                                                                    tx.send(VoiceCmd::SetUserVolume(p.user_id, 0.0)).ok();
                                                                }
                                                            }
                                                        }
                                                    });
                                                }
                                                // FPS overlay: отрисованные кадры/сек (не полученные/декодированные)
                                                if p.streaming {
                                                    let fps = state.main.voice_render_fps.get_mut(&key).map(|t| t.update_and_get()).unwrap_or(0.0);
                                                    if fps > 0.0 {
                                                        let fps_text = format!("{:.0} fps", fps);
                                                        let pos = avatar_rect.left_bottom() + egui::vec2(4.0, -18.0);
                                                        let size = egui::vec2(36.0, 16.0);
                                                        let bg_rect = egui::Rect::from_min_size(pos, size);
                                                        ui.painter().rect_filled(bg_rect, egui::Rounding::same(2.0), egui::Color32::from_black_alpha(180));
                                                        ui.painter().text(
                                                            pos + egui::vec2(4.0, 2.0),
                                                            egui::Align2::LEFT_TOP,
                                                            fps_text,
                                                            egui::FontId::proportional(11.0),
                                                            egui::Color32::WHITE,
                                                        );
                                                    }
                                                }
                                        }
                                        if tex_key.is_none() {
                                            // No video texture — show rounded rect avatar (letter)
                                            ui.painter().rect_filled(
                                                avatar_rect,
                                                egui::Rounding::same(ROUNDING),
                                                egui::Color32::from_rgb(80, 100, 160),
                                            );
                                            if is_speaking {
                                                ui.painter().rect_stroke(
                                                    avatar_rect.expand(2.0),
                                                    egui::Rounding::same(ROUNDING + 2.0),
                                                    egui::Stroke::new(2.0, egui::Color32::from_rgb(67, 181, 129)),
                                                );
                                            }
                                            let letter = p.username.chars().next()
                                                .map(|c| c.to_uppercase().to_string())
                                                .unwrap_or_else(|| "?".to_string());
                                            let font_size = (avatar_rect.height() * 0.4).max(14.0);
                                            let galley = ui.painter().layout(
                                                letter,
                                                egui::FontId::proportional(font_size),
                                                egui::Color32::WHITE,
                                                f32::INFINITY,
                                            );
                                            let pos = avatar_rect.center() - galley.size() / 2.0;
                                            ui.painter().galley(pos, galley, egui::Color32::WHITE);
                                        }

                                        // Name and icons below avatar area
                                        let name_y = avatar_rect.bottom() + 4.0;
                                        let name_rect = egui::Rect::from_min_max(
                                            rect.left_top() + egui::vec2(content_margin, name_y),
                                            rect.right_top() + egui::vec2(-content_margin, name_y + 18.0),
                                        );
                                        let color = if is_locally_muted {
                                            ui.visuals().weak_text_color()
                                        } else {
                                            ui.visuals().text_color()
                                        };
                                        let name = p.username.as_str();
                                        let max_chars = (tile_w / 7.0).max(8.0) as usize;
                                        let trunc = if name.len() > max_chars { format!("{}…", &name[..max_chars]) } else { name.to_string() };
                                        ui.painter().text(
                                            name_rect.left_center(),
                                            egui::Align2::LEFT_CENTER,
                                            trunc,
                                            egui::FontId::proportional(12.0),
                                            color,
                                        );
                                        let icon_y = name_y + 10.0;
                                        let mut icon_x = rect.left_top().x + content_margin;
                                        if p.mic_muted {
                                            ui.painter().text(egui::pos2(icon_x, icon_y), egui::Align2::LEFT_TOP, "🔇", egui::FontId::proportional(10.0), color);
                                            icon_x += 14.0;
                                        }
                                        if p.cam_enabled {
                                            ui.painter().text(egui::pos2(icon_x, icon_y), egui::Align2::LEFT_TOP, "📷", egui::FontId::proportional(10.0), color);
                                            icon_x += 14.0;
                                        }
                                        if p.streaming {
                                            ui.painter().text(egui::pos2(icon_x, icon_y), egui::Align2::LEFT_TOP, "📺", egui::FontId::proportional(10.0), color);
                                        }

                                        if in_this_voice {
                                            resp.context_menu(|ui| {
                                                let mute_label = if is_locally_muted { "Снять локальный мут" } else { "Заглушить локально" };
                                                if ui.button(mute_label).clicked() {
                                                    if is_locally_muted {
                                                        state.main.voice.locally_muted.remove(&p.user_id);
                                                        let restore = state.main.voice.local_volumes.get(&p.user_id).copied().unwrap_or(1.0);
                                                        if let Some(tx) = engine_tx.as_ref() {
                                                            tx.send(VoiceCmd::SetUserVolume(p.user_id, restore)).ok();
                                                        }
                                                    } else {
                                                        state.main.voice.locally_muted.insert(p.user_id);
                                                        if let Some(tx) = engine_tx.as_ref() {
                                                            tx.send(VoiceCmd::SetUserVolume(p.user_id, 0.0)).ok();
                                                        }
                                                    }
                                                    ui.close_menu();
                                                }
                                                ui.label("Громкость 0–300%, по умолчанию 100%");
                                                let uid = p.user_id;
                                                let mut vol = *state.main.voice.local_volumes.get(&uid).unwrap_or(&1.0);
                                                if ui.add(egui::Slider::new(&mut vol, 0.0..=3.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)).text("")).changed() {
                                                    state.main.voice.local_volumes.insert(uid, vol);
                                                    state.settings.voice_volume_by_user.insert(uid.to_string(), vol);
                                                    state.settings.save();
                                                    if let Some(tx) = engine_tx.as_ref() {
                                                        tx.send(VoiceCmd::SetUserVolume(uid, vol)).ok();
                                                    }
                                                }
                                            });
                                        }
                                    });
                                }
                            });
                            ui.add_space(4.0);
                        }
                    });

                // Buttons under grid: Mic, Camera, Screen share, Disconnect
                if in_this_voice {
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let mic_label = if state.main.voice.mic_muted { "🎤 Выкл" } else { "🎤 Вкл" };
                        if ui.button(mic_label).on_hover_text(if state.main.voice.mic_muted { "Включить микрофон" } else { "Выключить микрофон" }).clicked() {
                            state.main.voice.mic_muted = !state.main.voice.mic_muted;
                            if let Some(tx) = engine_tx.as_ref() {
                                tx.send(VoiceCmd::SetMicMuted(state.main.voice.mic_muted)).ok();
                            }
                            if let (Some(ch_id), Some(ref token)) = (state.main.voice.channel_id, &state.access_token) {
                                let token = token.clone();
                                let _ = block_on(api.voice_update_state(&token, ch_id, state.main.voice.mic_muted, state.main.voice.camera_on, state.main.voice.screen_on));
                            }
                            if let Some(uid) = state.user_id {
                                for p in &mut state.main.voice.participants {
                                    if p.user_id == uid { p.mic_muted = state.main.voice.mic_muted; break; }
                                }
                                if let Some(ch_id) = state.main.voice.channel_id {
                                    if let Some(list) = state.main.channel_voice.get_mut(&ch_id) {
                                        for p in list.iter_mut() {
                                            if p.user_id == uid { p.mic_muted = state.main.voice.mic_muted; break; }
                                        }
                                    }
                                }
                            }
                            ui.ctx().request_repaint();
                        }
                        let cam_label = if state.main.voice.camera_on { "📷 Камера вкл" } else { "📷 Камера" };
                        if ui.button(cam_label).on_hover_text(if state.main.voice.camera_on { "Выключить камеру" } else { "Включить камеру" }).clicked() {
                            state.main.voice.camera_on = !state.main.voice.camera_on;
                            if let Some(tx) = engine_tx.as_ref() {
                                if state.main.voice.camera_on { tx.send(VoiceCmd::StartCamera).ok(); } else { tx.send(VoiceCmd::StopCamera).ok(); }
                            }
                            if let (Some(ch_id), Some(ref token)) = (state.main.voice.channel_id, &state.access_token) {
                                let token = token.clone();
                                let _ = block_on(api.voice_update_state(&token, ch_id, state.main.voice.mic_muted, state.main.voice.camera_on, state.main.voice.screen_on));
                            }
                            if let Some(uid) = state.user_id {
                                for p in &mut state.main.voice.participants {
                                    if p.user_id == uid { p.cam_enabled = state.main.voice.camera_on; break; }
                                }
                                if let Some(ch_id) = state.main.voice.channel_id {
                                    if let Some(list) = state.main.channel_voice.get_mut(&ch_id) {
                                        for p in list.iter_mut() {
                                            if p.user_id == uid { p.cam_enabled = state.main.voice.camera_on; break; }
                                        }
                                    }
                                }
                            }
                            ui.ctx().request_repaint();
                        }
                        let scr_label = if state.main.voice.screen_on { "📺 Демо вкл" } else { "📺 Демо экрана" };
                        if ui.button(scr_label).on_hover_text(if state.main.voice.screen_on { "Остановить демонстрацию" } else { "Демонстрация экрана" }).clicked() {
                            if state.main.voice.screen_on {
                                state.main.voice.screen_on = false;
                                if let Some(tx) = engine_tx.as_ref() {
                                    tx.send(VoiceCmd::StopScreen).ok();
                                }
                                if let (Some(ch_id), Some(ref token)) = (state.main.voice.channel_id, &state.access_token) {
                                    let token = token.clone();
                                    let _ = block_on(api.voice_update_state(&token, ch_id, state.main.voice.mic_muted, state.main.voice.camera_on, state.main.voice.screen_on));
                                }
                                if let Some(uid) = state.user_id {
                                    for p in &mut state.main.voice.participants {
                                        if p.user_id == uid { p.streaming = false; break; }
                                    }
                                    if let Some(ch_id) = state.main.voice.channel_id {
                                        if let Some(list) = state.main.channel_voice.get_mut(&ch_id) {
                                            for p in list.iter_mut() {
                                                if p.user_id == uid { p.streaming = false; break; }
                                            }
                                        }
                                    }
                                }
                            } else {
                                // Enumerate real displays (deduplicated) for the picker.
                                let monitors = crate::voice_livekit::enumerate_unique_screens();
                                state.main.screen_source_names = monitors
                                    .iter()
                                    .enumerate()
                                    .map(|(i, m)| {
                                        let tag = if m.is_primary() { " (осн.)" } else { "" };
                                        format!("Монитор {}{} {}×{}", i + 1, tag, m.width(), m.height())
                                    })
                                    .collect();
                                state.main.show_screen_source_picker = true;
                            }
                            ui.ctx().request_repaint();
                        }
                        if ui.button("📵 Отключиться").on_hover_text("Отключиться от канала").clicked() {
                            state.main.voice_pending_leave = true;
                            ui.ctx().request_repaint();
                        }
                        if ui.button("📊 Статистика").on_hover_text("Окно статистики трансляции").clicked() {
                            state.main.show_voice_stats_window = true;
                            ui.ctx().request_repaint();
                        }
                    });
                }
            }
        }
    });

    // Voice statistics window (separate window)
    if state.main.show_voice_stats_window {
        let stats = state.main.voice_stats.as_ref().map(|s| s.lock().clone()).unwrap_or_default();
        egui::Window::new("Статистика")
            .collapsible(false)
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                let fmt_rtt = stats.latency_rtt_ms.map(|x| format!("{:.0}", x)).unwrap_or_else(|| "—".to_string());
                let fmt_fps = stats.stream_fps.map(|x| format!("{:.1}", x)).unwrap_or_else(|| "—".to_string());
                let fmt_res = stats.resolution.map(|(w, h)| format!("{}×{}", w, h)).unwrap_or_else(|| "—".to_string());
                let fmt_fps2 = stats.frames_per_second.map(|x| format!("{:.1}", x)).unwrap_or_else(|| "—".to_string());
                let fmt_mbps = stats.connection_speed_mbps.map(|x| format!("{:.2}", x)).unwrap_or_else(|| "—".to_string());
                let fmt_in_mbps = stats.incoming_speed_mbps.map(|x| format!("{:.2}", x)).unwrap_or_else(|| "—".to_string());
                let fmt_enc = stats.encoding_path.as_deref().unwrap_or("—");
                let fmt_dec = stats.decoding_path.as_deref().unwrap_or("—");
                let fmt_threads = stats.encoder_threads.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string());
                let fmt_dec_threads = stats.decoder_threads.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string());
                ui.heading("Трансляция и соединение");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Задержка (RTT), мс:");
                    ui.label(&fmt_rtt);
                });
                ui.horizontal(|ui| {
                    ui.label("ФПС трансляции:");
                    ui.label(&fmt_fps);
                });
                ui.horizontal(|ui| {
                    ui.label("Разрешение:");
                    ui.label(&fmt_res);
                });
                ui.horizontal(|ui| {
                    ui.label("Кадры в секунду:");
                    ui.label(&fmt_fps2);
                });
                ui.horizontal(|ui| {
                    ui.label("Скорость отдачи, Мбит/с:");
                    ui.label(&fmt_mbps);
                });
                ui.horizontal(|ui| {
                    ui.label("Скорость приёма, Мбит/с:");
                    ui.label(&fmt_in_mbps);
                    ui.label("(прибл. по кадрам)");
                });
                ui.horizontal(|ui| {
                    ui.label("Кодирование:");
                    ui.label(fmt_enc);
                });
                ui.horizontal(|ui| {
                    ui.label("Декодирование:");
                    ui.label(fmt_dec);
                });
                ui.horizontal(|ui| {
                    ui.label("Потоки энкодера:");
                    ui.label(&fmt_threads);
                });
                ui.horizontal(|ui| {
                    ui.label("Потоки декодера:");
                    ui.label(&fmt_dec_threads);
                });
                ui.add_space(12.0);
                ui.separator();
                if ui.button("Закрыть").clicked() {
                    state.main.show_voice_stats_window = false;
                }
            });
    }

    // Screen source picker dialog (before starting stream).
    if state.main.show_screen_source_picker {
        let mut close_picker = false;
        let mut start_with_index: Option<usize> = None;
        let screen_names = state.main.screen_source_names.clone();
        let current_preset = state.main.screen_preset;
        egui::Window::new("Выбор источника трансляции")
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.heading("Качество трансляции");
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    for &preset in crate::voice::ScreenPreset::ALL {
                        let selected = preset == current_preset;
                        if ui.selectable_label(selected, preset.label()).clicked() {
                            state.main.screen_preset = preset;
                        }
                        ui.add_space(2.0);
                    }
                });
                ui.add_space(8.0);
                ui.separator();
                ui.heading("Экран");
                ui.add_space(4.0);
                if screen_names.is_empty() {
                    ui.label("Экраны не обнаружены.");
                } else {
                    ui.horizontal_wrapped(|ui| {
                        for (idx, name) in screen_names.iter().enumerate() {
                            if ui.button(name).clicked() {
                                start_with_index = Some(idx);
                                close_picker = true;
                            }
                            ui.add_space(4.0);
                        }
                    });
                }
                ui.separator();
                ui.heading("Приложение");
                ui.label("Выбор окна приложения будет добавлен в следующей версии.");
                ui.add_space(8.0);
                if ui.button("Отмена").clicked() {
                    close_picker = true;
                }
            });
        if close_picker {
            state.main.show_screen_source_picker = false;
            if let Some(idx) = start_with_index {
                let preset = state.main.screen_preset;
                state.main.voice.screen_on = true;
                if let Some(tx) = engine_tx.as_ref() {
                    tx.send(VoiceCmd::StartScreen { screen_index: Some(idx), preset }).ok();
                }
                if let (Some(ch_id), Some(ref token)) = (state.main.voice.channel_id, &state.access_token) {
                    let token = token.clone();
                    let _ = block_on(api.voice_update_state(&token, ch_id, state.main.voice.mic_muted, state.main.voice.camera_on, state.main.voice.screen_on));
                }
                if let Some(uid) = state.user_id {
                    for p in &mut state.main.voice.participants {
                        if p.user_id == uid { p.streaming = true; break; }
                    }
                    if let Some(ch_id) = state.main.voice.channel_id {
                        if let Some(list) = state.main.channel_voice.get_mut(&ch_id) {
                            for p in list.iter_mut() {
                                if p.user_id == uid { p.streaming = true; break; }
                            }
                        }
                    }
                }
            }
        }
    }

    // Fullscreen stream overlay (on top of everything)
    if let Some(uid) = state.main.fullscreen_stream_user {
        egui::Area::new(egui::Id::new("voice_fullscreen"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(0.0, 0.0))
            .constrain(true)
            .show(ctx, |ui| {
                // Window rect: always (0,0) origin (window-relative), size from viewport.
                // Fixes overlay "moving away" when window is moved and ensures it fits in window.
                let viewport = ctx.input(|i| i.viewport().inner_rect).unwrap_or_else(|| ctx.screen_rect());
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, viewport.size());
                ui.allocate_rect(screen, egui::Sense::hover()); // Ensure Area covers full window
                ui.painter().rect_filled(screen, egui::Rounding::ZERO, egui::Color32::from_black_alpha(240));
                let stream_key = video_frame_key(uid, true);
                // Phase 3.5: prefer GPU zero-copy texture, fall back to CPU-uploaded.
                let shown = if let Some(&(egui_tex_id, _, w, h)) = state.main.voice_video_gpu_textures.get(&stream_key) {
                    let max_w = screen.width();
                    let max_h = screen.height();
                    // Scale to fill window; allow upscaling when window is larger than video.
                    let scale = (max_w / w as f32).min(max_h / h as f32);
                    let size = egui::vec2(w as f32 * scale, h as f32 * scale);
                    let pos = screen.center() - size / 2.0;
                    let sized = egui::load::SizedTexture::new(
                        egui_tex_id,
                        egui::vec2(w as f32, h as f32),
                    );
                    ui.put(egui::Rect::from_min_size(pos, size), egui::Image::new(sized).fit_to_exact_size(size));
                    true
                } else if let Some(tex) = state.main.voice_video_textures.get(&stream_key) {
                    let tex_size = tex.size_vec2();
                    let max_w = screen.width();
                    let max_h = screen.height();
                    // Scale to fill window; allow upscaling when window is larger than video.
                    let scale = (max_w / tex_size.x).min(max_h / tex_size.y);
                    let size = tex_size * scale;
                    let pos = screen.center() - size / 2.0;
                    ui.put(egui::Rect::from_min_size(pos, size), egui::Image::new(tex).fit_to_exact_size(size));
                    true
                } else {
                    false
                };
                if !shown {
                    ui.centered_and_justified(|ui| {
                        ui.label("Трансляция недоступна");
                    });
                } else {
                    // FPS overlay: отрисованные кадры/сек (не полученные/декодированные)
                    let fps = state.main.voice_render_fps.get_mut(&stream_key).map(|t| t.update_and_get()).unwrap_or(0.0);
                    if fps > 0.0 {
                        let fps_text = format!("{:.0} fps", fps);
                        let pos = screen.left_bottom() + egui::vec2(16.0, -28.0);
                        let size = egui::vec2(48.0, 22.0);
                        let bg_rect = egui::Rect::from_min_size(pos, size);
                        ui.painter().rect_filled(bg_rect, egui::Rounding::same(4.0), egui::Color32::from_black_alpha(200));
                        ui.painter().text(
                            pos + egui::vec2(8.0, 4.0),
                            egui::Align2::LEFT_TOP,
                            fps_text,
                            egui::FontId::proportional(14.0),
                            egui::Color32::WHITE,
                        );
                    }
                }
                let close_rect = egui::Rect::from_min_size(screen.left_top() + egui::vec2(16.0, 16.0), egui::vec2(140.0, 36.0));
                ui.allocate_ui_at_rect(close_rect, |ui| {
                    if ui.button("⛶ Закрыть полноэкранный режим").clicked() {
                        state.main.fullscreen_stream_user = None;
                    }
                });
            });
    }

    should_logout
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn mime_from_path(path: &PathBuf) -> String {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
        Some("png")         => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif")         => "image/gif",
        Some("webp")        => "image/webp",
        Some("mp4")         => "video/mp4",
        Some("webm")        => "video/webm",
        Some("avi")         => "video/avi",
        Some("zip")         => "application/zip",
        Some("rar")         => "application/x-rar-compressed",
        Some("7z")          => "application/x-7z-compressed",
        Some("tar")         => "application/x-tar",
        Some("gz")          => "application/gzip",
        Some("pdf")         => "application/pdf",
        _                   => "application/octet-stream",
    }.to_string()
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

fn save_media_to_disk(media_id: i64, filename: &str, media_bytes: &HashMap<i64, (Vec<u8>, String)>) {
    if let Some((bytes, _)) = media_bytes.get(&media_id) {
        if let Some(save_path) = rfd::FileDialog::new().set_file_name(filename).save_file() {
            let _ = std::fs::write(save_path, bytes);
        }
    }
}
