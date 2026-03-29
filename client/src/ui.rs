use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use std::sync::OnceLock;
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
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use egui_glow;
use parking_lot::Mutex;

use crate::bottom_panel::{self, BottomPanelAction, BottomPanelParams, BottomPanelVoiceSnapshot};
use crate::channel_panel::{
    self, ChannelPanelAction, ChannelPanelParams, ChannelPanelVoiceSnapshot, ChannelsLoadState,
};
use crate::chat_panel::{self, ChatPanelAction, ChatPanelParams};
use crate::crypto::ChannelKey;
use crate::guild_panel::{self, GuildPanelParams};
use crate::member_panel::{self, MemberPanelAction, MemberPanelParams, MemberSnapshot};
use crate::net::{
    new_event_queue, ws_task, ApiClient, AttachmentMeta, Channel, LoginRequest, Member, Message,
    RegisterRequest, Server, VoiceParticipant, WsClientMsg, WsEventQueue,
};
use crate::telemetry::PipelineTelemetry;
use crate::theme::Theme;
use crate::todo_actions;
use crate::voice::{
    spawn_voice_engine, video_frame_key, video_preview_frame_key, StreamSourceTarget, VideoFrame,
    VideoFrames, VoiceCmd, VoiceSessionStats,
};

// ─── Persistent settings (saved to disk) ────────────────────────────────────
// Used by app.rs; path and struct duplicated there for now.

const SETTINGS_PATH: &str = "astrix_settings.json";

fn default_api_base() -> String {
    crate::net::DEFAULT_API_BASE.to_string()
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct SavedAccount {
    #[serde(default)]
    pub(crate) user_id: Option<i64>,
    pub(crate) username: String,
    pub(crate) password: String,
    #[serde(default)]
    pub(crate) display_name: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct Settings {
    pub(crate) remember_me: bool,
    pub(crate) saved_username: String,
    pub(crate) saved_password: String,
    #[serde(default)]
    pub(crate) saved_accounts: Vec<SavedAccount>,
    #[serde(default = "default_api_base")]
    pub(crate) api_base: String,
    pub(crate) last_server: HashMap<String, i64>,
    #[serde(default)]
    pub(crate) dark_mode: bool,
    #[serde(default)]
    pub(crate) voice_volume_by_user: HashMap<String, f32>,
    #[serde(default)]
    pub(crate) stream_volume_by_user: HashMap<String, f32>,
    #[serde(default)]
    pub(crate) receiver_denoise_by_user: HashSet<String>,
    #[serde(default = "default_denoise_model_id")]
    pub(crate) denoise_model_id: String,
    #[serde(default = "default_denoise_atten_lim_db")]
    pub(crate) denoise_atten_lim_db: f32,
    #[serde(default = "default_denoise_post_filter_beta")]
    pub(crate) denoise_post_filter_beta: f32,
    #[serde(default = "default_denoise_min_db_thresh")]
    pub(crate) denoise_min_db_thresh: f32,
    #[serde(default = "default_denoise_max_db_erb_thresh")]
    pub(crate) denoise_max_db_erb_thresh: f32,
    #[serde(default = "default_denoise_max_db_df_thresh")]
    pub(crate) denoise_max_db_df_thresh: f32,
    #[serde(default = "default_denoise_reduce_mask")]
    pub(crate) denoise_reduce_mask: String,
    /// Путь декодирования входящего видео: "cpu" (OpenH264) или "mft" (Media Foundation).
    #[serde(default)]
    pub(crate) decode_path: String,
    /// Legacy gamma override for the GPU decode path. Normally keep 0; non-zero darkens the image.
    #[serde(default = "default_video_decoder_gamma")]
    pub(crate) video_decoder_gamma: f32,
    #[serde(default)]
    pub(crate) video_decoder_gamma_migrated_v2: bool,
}

fn default_video_decoder_gamma() -> f32 {
    0.0
}

fn default_denoise_model_id() -> String {
    crate::denoise::default_model_id().to_string()
}

fn default_denoise_atten_lim_db() -> f32 {
    crate::denoise::default_atten_lim_db()
}

fn default_denoise_post_filter_beta() -> f32 {
    crate::denoise::default_post_filter_beta()
}

fn default_denoise_min_db_thresh() -> f32 {
    crate::denoise::default_min_db_thresh()
}

fn default_denoise_max_db_erb_thresh() -> f32 {
    crate::denoise::default_max_db_erb_thresh()
}

fn default_denoise_max_db_df_thresh() -> f32 {
    crate::denoise::default_max_db_df_thresh()
}

fn default_denoise_reduce_mask() -> String {
    crate::denoise::default_reduce_mask_id().to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            remember_me: false,
            saved_username: String::new(),
            saved_password: String::new(),
            saved_accounts: Vec::new(),
            api_base: default_api_base(),
            last_server: HashMap::new(),
            dark_mode: false,
            voice_volume_by_user: HashMap::new(),
            stream_volume_by_user: HashMap::new(),
            receiver_denoise_by_user: HashSet::new(),
            denoise_model_id: default_denoise_model_id(),
            denoise_atten_lim_db: default_denoise_atten_lim_db(),
            denoise_post_filter_beta: default_denoise_post_filter_beta(),
            denoise_min_db_thresh: default_denoise_min_db_thresh(),
            denoise_max_db_erb_thresh: default_denoise_max_db_erb_thresh(),
            denoise_max_db_df_thresh: default_denoise_max_db_df_thresh(),
            denoise_reduce_mask: default_denoise_reduce_mask(),
            decode_path: String::new(),
            video_decoder_gamma: 0.0,
            video_decoder_gamma_migrated_v2: true,
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
        let mut should_save = false;
        if s.decode_path.is_empty() || (s.decode_path != "cpu" && s.decode_path != "mft") {
            s.decode_path = "mft".to_string();
            should_save = true;
        }
        if s.api_base.trim().is_empty() {
            s.api_base = default_api_base();
            should_save = true;
        }
        if !crate::denoise::is_known_model(&s.denoise_model_id) {
            s.denoise_model_id = default_denoise_model_id();
            should_save = true;
        }
        let normalized_atten_lim = crate::denoise::normalize_atten_lim_db(s.denoise_atten_lim_db);
        if (s.denoise_atten_lim_db - normalized_atten_lim).abs() > f32::EPSILON {
            s.denoise_atten_lim_db = normalized_atten_lim;
            should_save = true;
        }
        let normalized_post_filter =
            crate::denoise::normalize_post_filter_beta(s.denoise_post_filter_beta);
        if (s.denoise_post_filter_beta - normalized_post_filter).abs() > f32::EPSILON {
            s.denoise_post_filter_beta = normalized_post_filter;
            should_save = true;
        }
        let normalized_min_thresh =
            crate::denoise::normalize_min_db_thresh(s.denoise_min_db_thresh);
        if (s.denoise_min_db_thresh - normalized_min_thresh).abs() > f32::EPSILON {
            s.denoise_min_db_thresh = normalized_min_thresh;
            should_save = true;
        }
        let normalized_max_erb =
            crate::denoise::normalize_max_db_erb_thresh(s.denoise_max_db_erb_thresh);
        if (s.denoise_max_db_erb_thresh - normalized_max_erb).abs() > f32::EPSILON {
            s.denoise_max_db_erb_thresh = normalized_max_erb;
            should_save = true;
        }
        let normalized_max_df =
            crate::denoise::normalize_max_db_df_thresh(s.denoise_max_db_df_thresh);
        if (s.denoise_max_db_df_thresh - normalized_max_df).abs() > f32::EPSILON {
            s.denoise_max_db_df_thresh = normalized_max_df;
            should_save = true;
        }
        if !crate::denoise::is_known_reduce_mask(&s.denoise_reduce_mask) {
            s.denoise_reduce_mask = default_denoise_reduce_mask();
            should_save = true;
        }
        if s.migrate_saved_accounts() {
            should_save = true;
        }
        if !s.video_decoder_gamma_migrated_v2 {
            if s.video_decoder_gamma > 0.0 && s.video_decoder_gamma <= 0.75 {
                eprintln!(
                    "[video] resetting legacy decoder gamma workaround {:.2} -> 0.00 after GPU video color-path fix",
                    s.video_decoder_gamma
                );
                s.video_decoder_gamma = 0.0;
            }
            s.video_decoder_gamma_migrated_v2 = true;
            should_save = true;
        }
        s.video_decoder_gamma = s.video_decoder_gamma.clamp(0.0, 3.0);
        #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
        crate::d3d11_rgba::set_video_decoder_gamma(s.video_decoder_gamma);
        crate::denoise::set_selected_model(&s.denoise_model_id);
        crate::denoise::set_denoise_atten_lim_db(s.denoise_atten_lim_db);
        crate::denoise::set_denoise_post_filter_beta(s.denoise_post_filter_beta);
        crate::denoise::set_denoise_thresholds(
            s.denoise_min_db_thresh,
            s.denoise_max_db_erb_thresh,
            s.denoise_max_db_df_thresh,
        );
        crate::denoise::set_denoise_reduce_mask(&s.denoise_reduce_mask);
        if should_save {
            s.save();
        }
        s
    }
    pub(crate) fn save(&self) {
        let _ = std::fs::write(
            SETTINGS_PATH,
            serde_json::to_string_pretty(self).unwrap_or_default(),
        );
    }

    fn migrate_saved_accounts(&mut self) -> bool {
        let mut changed = false;

        if !self.saved_username.trim().is_empty() && !self.saved_password.is_empty() {
            let exists = self.saved_accounts.iter().any(|account| {
                account
                    .username
                    .eq_ignore_ascii_case(self.saved_username.trim())
            });
            if !exists {
                self.saved_accounts.insert(
                    0,
                    SavedAccount {
                        user_id: None,
                        username: self.saved_username.clone(),
                        password: self.saved_password.clone(),
                        display_name: self.saved_username.clone(),
                    },
                );
                changed = true;
            }
        }

        let old_len = self.saved_accounts.len();
        self.saved_accounts
            .retain(|account| !account.username.trim().is_empty() && !account.password.is_empty());
        if self.saved_accounts.len() != old_len {
            changed = true;
        }

        let mut deduped = Vec::with_capacity(self.saved_accounts.len());
        for account in self.saved_accounts.drain(..) {
            let duplicate = deduped.iter().any(|existing: &SavedAccount| {
                existing.user_id == account.user_id
                    || existing
                        .username
                        .eq_ignore_ascii_case(account.username.trim())
            });
            if duplicate {
                changed = true;
            } else {
                deduped.push(account);
            }
        }
        self.saved_accounts = deduped;

        if self.saved_accounts.len() > 8 {
            self.saved_accounts.truncate(8);
            changed = true;
        }

        if self.remember_me
            && (self.saved_username.trim().is_empty() || self.saved_password.is_empty())
            && !self.saved_accounts.is_empty()
        {
            if let Some(account) = self.saved_accounts.first() {
                self.saved_username = account.username.clone();
                self.saved_password = account.password.clone();
                changed = true;
            }
        }

        changed
    }

    fn upsert_saved_account(
        &mut self,
        user_id: i64,
        username: String,
        password: String,
        display_name: String,
    ) {
        self.saved_accounts.retain(|account| {
            account.user_id != Some(user_id)
                && !account.username.eq_ignore_ascii_case(username.trim())
        });
        self.saved_accounts.insert(
            0,
            SavedAccount {
                user_id: Some(user_id),
                username: username.clone(),
                password: password.clone(),
                display_name: if display_name.trim().is_empty() {
                    username.clone()
                } else {
                    display_name
                },
            },
        );
        if self.saved_accounts.len() > 8 {
            self.saved_accounts.truncate(8);
        }
        self.saved_username = username;
        self.saved_password = password;
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
        let do_channels = (st.main.channels_load == LoadState::Idle
            && st.main.selected_server.is_some())
            || st.main.retry_channels
            || (st.main.selected_server != st.main.channels_load_for
                && st.main.selected_server.is_some());
        let do_messages = (st.main.messages_load == LoadState::Idle
            && st.main.selected_channel.is_some())
            || st.main.retry_messages
            || (st.main.selected_channel != st.main.messages_load_for
                && st.main.selected_channel.is_some());
        let server_id = st.main.selected_server;
        let channel_id = st.main.selected_channel;
        let user_id = st.user_id;
        let last_server = st.settings.last_server.clone();
        (
            do_servers,
            do_channels,
            do_messages,
            token,
            server_id,
            channel_id,
            user_id,
            last_server,
        )
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
                Err(_) => {
                    st.main.servers_load = LoadState::Error("Таймаут загрузки серверов".to_string())
                }
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
            let chs_result =
                tokio::time::timeout(Duration::from_secs(LOAD_TIMEOUT_SECS), chs_fut).await;
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
                    st.main.channels_load =
                        LoadState::Error("Таймаут загрузки каналов".to_string());
                    ctx_c.request_repaint();
                    return;
                }
            };
            let voice_ch_ids: Vec<i64> = channels
                .iter()
                .filter(|c| c.r#type == "voice")
                .map(|c| c.id)
                .collect();
            let mut channel_voice = HashMap::new();
            for ch_id in voice_ch_ids {
                if let Ok(Ok(ps)) =
                    tokio::time::timeout(Duration::from_secs(5), api_c.voice_state(&token_c, ch_id))
                        .await
                {
                    if !ps.is_empty() {
                        channel_voice.insert(ch_id, ps);
                    }
                }
            }
            let ms_fut = api_c.list_server_members(&token_c, sid);
            let ms_result =
                tokio::time::timeout(Duration::from_secs(LOAD_TIMEOUT_SECS), ms_fut).await;
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
                    st.main.channels_load =
                        LoadState::Error("Таймаут загрузки участников".to_string());
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
            st.main
                .channels
                .iter()
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
                    st.main.unread_channels.remove(&cid);
                    if max_id > 0 {
                        st.main.pending_read_receipt = Some((cid, max_id));
                    }
                    st.main.pending_media_ids = st
                        .main
                        .messages
                        .iter()
                        .flat_map(|m| m.attachments.iter().map(|a| a.media_id))
                        .collect();
                    st.main.messages_load = LoadState::Loaded;
                }
                Ok(Err(e)) => st.main.messages_load = LoadState::Error(e.to_string()),
                Err(_) => {
                    st.main.messages_load =
                        LoadState::Error("Таймаут загрузки сообщений".to_string())
                }
            }
            ctx_c.request_repaint();
        });
    }
}

// ─── State (pub(crate) for app.rs) ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Screen {
    Auth,
    Main,
}
impl Default for Screen {
    fn default() -> Self {
        Screen::Auth
    }
}

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
    pub(crate) stream_volumes: HashMap<i64, f32>,
    pub(crate) stream_muted: HashSet<i64>,
    pub(crate) stream_subscriptions: HashSet<i64>,
    pub(crate) receiver_denoise_users: HashSet<i64>,
    pub(crate) speaking: Arc<Mutex<HashMap<i64, bool>>>,
    pub(crate) input_volume: f32,
    pub(crate) output_volume: f32,
    pub(crate) camera_on: bool,
    pub(crate) screen_on: bool,
    pub(crate) screen_audio_muted: bool,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            channel_id: None,
            server_id: None,
            participants: Vec::new(),
            mic_muted: false,
            output_muted: false,
            local_volumes: HashMap::new(),
            locally_muted: HashSet::new(),
            stream_volumes: HashMap::new(),
            stream_muted: HashSet::new(),
            stream_subscriptions: HashSet::new(),
            receiver_denoise_users: HashSet::new(),
            speaking: Arc::new(Mutex::new(HashMap::new())),
            input_volume: 1.0,
            output_volume: 1.0,
            camera_on: false,
            screen_on: false,
            screen_audio_muted: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScreenSourceEntry {
    pub(crate) label: String,
    pub(crate) target: StreamSourceTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenSourceTab {
    Applications,
    EntireScreen,
}

impl Default for ScreenSourceTab {
    fn default() -> Self {
        Self::Applications
    }
}

pub(crate) struct MainState {
    pub(crate) servers: Vec<Server>,
    pub(crate) channels: Vec<Channel>,
    pub(crate) messages: Vec<Message>,
    pub(crate) server_members: Vec<Member>,
    pub(crate) unread_channels: HashSet<i64>,
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
    pub(crate) screen_sources: Vec<ScreenSourceEntry>,
    pub(crate) window_sources: Vec<ScreenSourceEntry>,
    pub(crate) selected_stream_source: Option<ScreenSourceEntry>,
    pub(crate) start_stream_after_source_pick: bool,
    pub(crate) screen_source_tab: ScreenSourceTab,
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
            unread_channels: self.unread_channels.clone(),
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
            screen_sources: Vec::new(),
            window_sources: Vec::new(),
            selected_stream_source: self.selected_stream_source.clone(),
            start_stream_after_source_pick: false,
            screen_source_tab: self.screen_source_tab,
            screen_preset: self.screen_preset,
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
            unread_channels: HashSet::new(),
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
            screen_sources: Vec::new(),
            window_sources: Vec::new(),
            selected_stream_source: None,
            start_stream_after_source_pick: false,
            screen_source_tab: ScreenSourceTab::default(),
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
    let mut pending_login: Option<(String, String, bool, bool)> = None;
    let saved_accounts = state.settings.saved_accounts.clone();

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading(egui::RichText::new("Astrix").size(32.0));
            ui.add_space(24.0);

            ui.label(if state.auth.is_register {
                "Регистрация"
            } else {
                "Вход"
            });
            ui.add_space(8.0);

            if !state.auth.is_register && !saved_accounts.is_empty() {
                ui.label("Сохраненные аккаунты");
                ui.horizontal_wrapped(|ui| {
                    for account in &saved_accounts {
                        let display_name = if account.display_name.trim().is_empty() {
                            account.username.as_str()
                        } else {
                            account.display_name.as_str()
                        };
                        let first = display_name
                            .chars()
                            .next()
                            .map(|ch| ch.to_uppercase().to_string())
                            .unwrap_or_else(|| "?".to_string());
                        let selected = state.auth.username.eq_ignore_ascii_case(&account.username);
                        if letter_circle(
                            ui,
                            &first,
                            20.0,
                            selected,
                            &format!("Войти как {}", display_name),
                        )
                        .clicked()
                        {
                            state.auth.username = account.username.clone();
                            state.auth.password = account.password.clone();
                            state.auth.remember_me = true;
                            state.auth.error = None;
                            pending_login = Some((
                                account.username.clone(),
                                account.password.clone(),
                                false,
                                true,
                            ));
                        }
                        ui.add_space(4.0);
                    }
                });
                ui.add_space(12.0);
            }

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

            if ui
                .button(if state.auth.is_register {
                    "Зарегистрироваться"
                } else {
                    "Войти"
                })
                .clicked()
            {
                let username = state.auth.username.clone();
                let password = state.auth.password.clone();
                let is_register = state.auth.is_register;
                let api = api.clone();

                let result = block_on(async move {
                    if is_register {
                        let req = RegisterRequest {
                            username: username.clone(),
                            password: password.clone(),
                            public_e2ee_key: None,
                        };
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
                            state.settings.upsert_saved_account(
                                tokens.user_id,
                                state.auth.username.clone(),
                                state.auth.password.clone(),
                                tokens.username.clone(),
                            );
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
            if ui
                .button(if state.auth.is_register {
                    "У меня уже есть аккаунт"
                } else {
                    "Создать аккаунт"
                })
                .clicked()
            {
                state.auth.is_register = !state.auth.is_register;
                state.auth.error = None;
            }

            if let Some(err) = &state.auth.error {
                ui.colored_label(egui::Color32::RED, err);
            }
        });
    });

    if let Some((username, password, is_register, remember_me)) = pending_login {
        let api = api.clone();
        let result = block_on(async move {
            if is_register {
                let req = RegisterRequest {
                    username: username.clone(),
                    password: password.clone(),
                    public_e2ee_key: None,
                };
                api.register(&req).await?;
                let login_req = LoginRequest { username, password };
                api.login(&login_req).await
            } else {
                api.login(&LoginRequest { username, password }).await
            }
        });

        match result {
            Ok(tokens) => {
                if remember_me {
                    state.settings.remember_me = true;
                    state.settings.upsert_saved_account(
                        tokens.user_id,
                        state.auth.username.clone(),
                        state.auth.password.clone(),
                        tokens.username.clone(),
                    );
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

                if let Some(sid) = last_server {
                    state.main.selected_server = Some(sid);
                }
                ctx.request_repaint();
            }
            Err(e) => {
                error = Some(format!("РћС€РёР±РєР° Р°РІС‚РѕСЂРёР·Р°С†РёРё: {e}"));
            }
        }
    }

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
    let resp = if tooltip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tooltip)
    };
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
    let circle_rect =
        egui::Rect::from_center_size(rect.center(), egui::vec2(radius * 2.0, radius * 2.0));
    if let Some(tex) = texture {
        let img = egui::Image::new(tex).fit_to_exact_size(circle_rect.size());
        img.paint_at(ui, circle_rect);
    } else {
        let letter = display_name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());
        ui.painter()
            .circle_filled(rect.center(), radius, egui::Color32::from_rgb(80, 100, 160));
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
    engine_done: &mut Option<std::sync::mpsc::Receiver<()>>,
    video_frames: &mut Option<VideoFrames>,
    channel_id: i64,
    server_id: i64,
    user_id: Option<i64>,
) {
    if let Some(ref token) = state.access_token {
        let token = token.clone();
        match block_on(api.voice_join(&token, channel_id, server_id)) {
            Ok(resp) => {
                state.main.voice_pending_leave = false;
                state.main.voice.channel_id = Some(channel_id);
                state.main.voice.server_id = Some(server_id);
                state.main.voice.participants = resp.participants.clone();
                state
                    .main
                    .channel_voice
                    .insert(channel_id, resp.participants.clone());
                state.main.voice.local_volumes.clear();
                state.main.voice.locally_muted.clear();
                state.main.voice.stream_volumes.clear();
                state.main.voice.stream_muted.clear();
                state.main.voice.stream_subscriptions.clear();
                for p in &resp.participants {
                    let vol = state
                        .settings
                        .voice_volume_by_user
                        .get(&p.user_id.to_string())
                        .copied()
                        .unwrap_or(1.0);
                    let stream_vol = state
                        .settings
                        .stream_volume_by_user
                        .get(&p.user_id.to_string())
                        .copied()
                        .unwrap_or(1.0);
                    state.main.voice.local_volumes.insert(p.user_id, vol);
                    state
                        .main
                        .voice
                        .stream_volumes
                        .insert(p.user_id, stream_vol);
                }
                state.main.voice.mic_muted = false;
                stop_voice_engine(engine_tx, engine_done);
                // Priority: external env var > UI settings > default "mft".
                // External env var is read here (before we overwrite it below).
                // This allows `$env:ASTRIX_DECODE_PATH=mft cargo run` to override
                // any saved setting, and running the exe directly uses the saved
                // setting (or the "mft" default if none is saved).
                let env_override = std::env::var("ASTRIX_DECODE_PATH").unwrap_or_default();
                let decode_path_owned: String = if env_override == "mft" || env_override == "cpu" {
                    env_override
                } else if state.settings.decode_path == "cpu" || state.settings.decode_path == "mft"
                {
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
                let (tx, vf, done) = spawn_voice_engine(rt);
                *video_frames = Some(vf);
                let receiver_telemetry = Arc::new(PipelineTelemetry::new());
                state.main.voice_receiver_telemetry = Some(Arc::clone(&receiver_telemetry));
                let session_stats = state
                    .main
                    .voice_stats
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
                })
                .ok();
                tx.send(VoiceCmd::SetInputVolume(state.main.voice.input_volume))
                    .ok();
                tx.send(VoiceCmd::SetOutputVolume(state.main.voice.output_volume))
                    .ok();
                tx.send(VoiceCmd::SetScreenAudioMuted(
                    state.main.voice.screen_audio_muted,
                ))
                .ok();
                for participant in &resp.participants {
                    let user_volume = state
                        .main
                        .voice
                        .local_volumes
                        .get(&participant.user_id)
                        .copied()
                        .unwrap_or(1.0);
                    let stream_volume = state
                        .main
                        .voice
                        .stream_volumes
                        .get(&participant.user_id)
                        .copied()
                        .unwrap_or(1.0);
                    tx.send(VoiceCmd::SetUserVolume(participant.user_id, user_volume))
                        .ok();
                    tx.send(VoiceCmd::SetStreamVolume(
                        participant.user_id,
                        stream_volume,
                    ))
                    .ok();
                    let denoise_enabled = state
                        .settings
                        .receiver_denoise_by_user
                        .contains(&participant.user_id.to_string());
                    if denoise_enabled {
                        state
                            .main
                            .voice
                            .receiver_denoise_users
                            .insert(participant.user_id);
                    } else {
                        state
                            .main
                            .voice
                            .receiver_denoise_users
                            .remove(&participant.user_id);
                    }
                    tx.send(VoiceCmd::SetRemoteVoiceDenoise {
                        user_id: participant.user_id,
                        enabled: denoise_enabled,
                    })
                    .ok();
                }
                *engine_tx = Some(tx);
                *engine_done = Some(done);
            }
            Err(e) => eprintln!("voice join error: {e}"),
        }
        ctx.request_repaint();
    }
}

fn stop_voice_engine(
    engine_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    engine_done: &mut Option<std::sync::mpsc::Receiver<()>>,
) {
    if let Some(tx) = engine_tx.take() {
        tx.send(VoiceCmd::Stop).ok();
    }
    if let Some(done_rx) = engine_done.take() {
        match done_rx.recv_timeout(Duration::from_millis(5000)) {
            Ok(()) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                eprintln!("[voice][livekit] previous engine did not stop within 5000 ms");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        }
    }
}

/// Отключение от голосового канала и остановка движка.
fn apply_voice_leave(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: &mut Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    engine_done: &mut Option<std::sync::mpsc::Receiver<()>>,
    video_frames: &mut Option<VideoFrames>,
) {
    state.main.voice_pending_leave = false;
    if let (Some(ch_id), Some(ref token)) = (state.main.voice.channel_id, &state.access_token) {
        let token = token.clone();
        let _ = block_on(api.voice_leave(&token, ch_id));
    }
    stop_voice_engine(engine_tx, engine_done);
    state.main.voice = VoiceState::default();
    state.main.show_screen_source_picker = false;
    state.main.start_stream_after_source_pick = false;
    state.main.voice_video_textures.clear();
    state.main.voice_render_fps.clear();
    state.main.voice_receiver_telemetry = None;
    // Phase 3.5: schedule texture deletion (freed via tex_manager in next update()).
    for (_, (egui_tex_id, _, _, _)) in state.main.voice_video_gpu_textures.drain() {
        state
            .main
            .voice_video_gpu_tex_pending_delete
            .push(egui_tex_id);
    }
    state.main.fullscreen_stream_user = None;
    state.main.stream_ended_prev_frame.clear();
    ctx.request_repaint();
}

fn sync_voice_presence(state: &State, api: &ApiClient) {
    if let (Some(ch_id), Some(ref token)) = (state.main.voice.channel_id, &state.access_token) {
        let token = token.clone();
        let _ = block_on(api.voice_update_state(
            &token,
            ch_id,
            state.main.voice.mic_muted,
            state.main.voice.camera_on,
            state.main.voice.screen_on,
        ));
    }
}

fn update_local_camera_flag(state: &mut State, user_id: Option<i64>, enabled: bool) {
    if let Some(uid) = user_id {
        for participant in &mut state.main.voice.participants {
            if participant.user_id == uid {
                participant.cam_enabled = enabled;
                break;
            }
        }
        if let Some(ch_id) = state.main.voice.channel_id {
            if let Some(list) = state.main.channel_voice.get_mut(&ch_id) {
                for participant in list.iter_mut() {
                    if participant.user_id == uid {
                        participant.cam_enabled = enabled;
                        break;
                    }
                }
            }
        }
    }
}

fn update_local_stream_flag(state: &mut State, user_id: Option<i64>, enabled: bool) {
    if let Some(uid) = user_id {
        for participant in &mut state.main.voice.participants {
            if participant.user_id == uid {
                participant.streaming = enabled;
                break;
            }
        }
        if let Some(ch_id) = state.main.voice.channel_id {
            if let Some(list) = state.main.channel_voice.get_mut(&ch_id) {
                for participant in list.iter_mut() {
                    if participant.user_id == uid {
                        participant.streaming = enabled;
                        break;
                    }
                }
            }
        }
    }
}

fn update_local_mic_flag(state: &mut State, user_id: Option<i64>, muted: bool) {
    if let Some(uid) = user_id {
        for participant in &mut state.main.voice.participants {
            if participant.user_id == uid {
                participant.mic_muted = muted;
                break;
            }
        }
        if let Some(ch_id) = state.main.voice.channel_id {
            if let Some(list) = state.main.channel_voice.get_mut(&ch_id) {
                for participant in list.iter_mut() {
                    if participant.user_id == uid {
                        participant.mic_muted = muted;
                        break;
                    }
                }
            }
        }
    }
}

fn populate_screen_share_sources(state: &mut State) {
    let monitors = crate::voice_livekit::enumerate_unique_screens();
    state.main.screen_sources = monitors
        .iter()
        .enumerate()
        .map(|(i, monitor)| {
            let tag = if monitor.is_primary() {
                " (осн.)"
            } else {
                ""
            };
            ScreenSourceEntry {
                label: format!(
                    "Монитор {}{} {}×{}",
                    i + 1,
                    tag,
                    monitor.width(),
                    monitor.height()
                ),
                target: StreamSourceTarget::Monitor { index: i },
            }
        })
        .collect();
    state.main.window_sources = crate::voice_livekit::enumerate_stream_windows()
        .into_iter()
        .map(|window| {
            let title = window.title.trim();
            let suffix = if title.is_empty() {
                String::new()
            } else {
                format!(" - {}", title)
            };
            ScreenSourceEntry {
                label: format!(
                    "{}{} {}×{}",
                    window.app_name, suffix, window.width, window.height
                ),
                target: StreamSourceTarget::Window {
                    window_id: window.window_id,
                    process_id: window.process_id,
                },
            }
        })
        .collect();
}

fn set_local_camera_enabled(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: Option<i64>,
    enabled: bool,
) {
    state.main.voice.camera_on = enabled;
    if let Some(tx) = engine_tx {
        if enabled {
            tx.send(VoiceCmd::StartCamera).ok();
        } else {
            tx.send(VoiceCmd::StopCamera).ok();
        }
    }
    sync_voice_presence(state, api);
    update_local_camera_flag(state, user_id, enabled);
    ctx.request_repaint();
}

fn set_local_mic_muted(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: Option<i64>,
    muted: bool,
) {
    state.main.voice.mic_muted = muted;
    if let Some(tx) = engine_tx {
        tx.send(VoiceCmd::SetMicMuted(muted)).ok();
    }
    sync_voice_presence(state, api);
    update_local_mic_flag(state, user_id, muted);
    ctx.request_repaint();
}

fn set_local_output_muted(
    ctx: &egui::Context,
    state: &mut State,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    muted: bool,
) {
    state.main.voice.output_muted = muted;
    if let Some(tx) = engine_tx {
        tx.send(VoiceCmd::SetOutputMuted(muted)).ok();
    }
    ctx.request_repaint();
}

fn set_local_deafened(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: Option<i64>,
    deafened: bool,
) {
    set_local_output_muted(ctx, state, engine_tx, deafened);
    set_local_mic_muted(ctx, state, api, engine_tx, user_id, deafened);
}

fn open_screen_share_picker(ctx: &egui::Context, state: &mut State, start_after_pick: bool) {
    populate_screen_share_sources(state);
    state.main.start_stream_after_source_pick = start_after_pick;
    state.main.screen_source_tab = if state.main.window_sources.is_empty() {
        ScreenSourceTab::EntireScreen
    } else {
        ScreenSourceTab::Applications
    };
    state.main.show_screen_source_picker = true;
    ctx.request_repaint();
}

fn start_screen_share_from_source(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: Option<i64>,
    source: StreamSourceTarget,
) {
    let preset = state.main.screen_preset;
    state.main.voice.screen_on = true;
    if let Some(tx) = engine_tx {
        tx.send(VoiceCmd::StartScreen { source, preset }).ok();
    }
    sync_voice_presence(state, api);
    update_local_stream_flag(state, user_id, true);
    ctx.request_repaint();
}

fn start_screen_share_from_selected_source(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: Option<i64>,
) -> bool {
    let Some(source) = state
        .main
        .selected_stream_source
        .as_ref()
        .map(|entry| entry.target.clone())
    else {
        return false;
    };

    start_screen_share_from_source(ctx, state, api, engine_tx, user_id, source);
    true
}

fn stop_local_screen_share(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: Option<i64>,
) {
    state.main.voice.screen_on = false;
    if let Some(tx) = engine_tx {
        tx.send(VoiceCmd::StopScreen).ok();
    }
    sync_voice_presence(state, api);
    update_local_stream_flag(state, user_id, false);
    ctx.request_repaint();
}

fn toggle_local_screen_share(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: Option<i64>,
) {
    if state.main.voice.screen_on {
        stop_local_screen_share(ctx, state, api, engine_tx, user_id);
        return;
    }

    if start_screen_share_from_selected_source(ctx, state, api, engine_tx, user_id) {
        return;
    }

    open_screen_share_picker(ctx, state, true);
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
struct WglVideoCallbackRenderer {
    program: eframe::glow::Program,
    vao: eframe::glow::VertexArray,
    vbo: eframe::glow::Buffer,
    u_sampler: Option<eframe::glow::UniformLocation>,
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
static WGL_VIDEO_CALLBACK_RENDERER: OnceLock<Mutex<Option<WglVideoCallbackRenderer>>> =
    OnceLock::new();

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn with_wgl_video_callback_renderer<R>(
    gl: &eframe::glow::Context,
    f: impl FnOnce(&eframe::glow::Context, &WglVideoCallbackRenderer) -> R,
) -> Result<R, String> {
    use eframe::glow::HasContext;

    const VERT_SRC: &str = r#"#version 330 core
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_uv;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    v_uv = a_uv;
}
"#;

    const FRAG_SRC: &str = r#"#version 330 core
uniform sampler2D u_sampler;
in vec2 v_uv;
out vec4 f_color;
void main() {
    vec4 c = texture(u_sampler, v_uv);
    f_color = vec4(c.rgb, 1.0);
}
"#;

    let renderer_lock = WGL_VIDEO_CALLBACK_RENDERER.get_or_init(|| Mutex::new(None));
    let mut guard = renderer_lock.lock();
    if guard.is_none() {
        let program = unsafe { gl.create_program() }
            .map_err(|e| format!("video callback create_program: {e}"))?;
        let vert = unsafe { gl.create_shader(eframe::glow::VERTEX_SHADER) }
            .map_err(|e| format!("video callback create vertex shader: {e}"))?;
        let frag = unsafe { gl.create_shader(eframe::glow::FRAGMENT_SHADER) }
            .map_err(|e| format!("video callback create fragment shader: {e}"))?;
        unsafe {
            gl.shader_source(vert, VERT_SRC);
            gl.compile_shader(vert);
            if !gl.get_shader_compile_status(vert) {
                let log = gl.get_shader_info_log(vert);
                gl.delete_shader(vert);
                gl.delete_shader(frag);
                gl.delete_program(program);
                return Err(format!("video callback vertex compile failed: {log}"));
            }

            gl.shader_source(frag, FRAG_SRC);
            gl.compile_shader(frag);
            if !gl.get_shader_compile_status(frag) {
                let log = gl.get_shader_info_log(frag);
                gl.delete_shader(vert);
                gl.delete_shader(frag);
                gl.delete_program(program);
                return Err(format!("video callback fragment compile failed: {log}"));
            }

            gl.attach_shader(program, vert);
            gl.attach_shader(program, frag);
            gl.link_program(program);
            gl.detach_shader(program, vert);
            gl.detach_shader(program, frag);
            gl.delete_shader(vert);
            gl.delete_shader(frag);
            if !gl.get_program_link_status(program) {
                let log = gl.get_program_info_log(program);
                gl.delete_program(program);
                return Err(format!("video callback link failed: {log}"));
            }
        }

        let vao = unsafe { gl.create_vertex_array() }
            .map_err(|e| format!("video callback create_vao: {e}"))?;
        let vbo =
            unsafe { gl.create_buffer() }.map_err(|e| format!("video callback create_vbo: {e}"))?;
        let vertices: [f32; 16] = [
            -1.0, -1.0, 0.0, 1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0,
        ];
        let mut vertex_bytes = Vec::with_capacity(vertices.len() * std::mem::size_of::<f32>());
        for value in vertices {
            vertex_bytes.extend_from_slice(&value.to_ne_bytes());
        }
        unsafe {
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(eframe::glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(
                eframe::glow::ARRAY_BUFFER,
                &vertex_bytes,
                eframe::glow::STATIC_DRAW,
            );
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, eframe::glow::FLOAT, false, 16, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, eframe::glow::FLOAT, false, 16, 8);
            gl.bind_buffer(eframe::glow::ARRAY_BUFFER, None);
            gl.bind_vertex_array(None);
        }

        if crate::telemetry::is_telemetry_enabled() {
            eprintln!("[Phase 3.5] WGL video callback renderer initialized");
        }
        *guard = Some(WglVideoCallbackRenderer {
            program,
            vao,
            vbo,
            u_sampler: unsafe { gl.get_uniform_location(program, "u_sampler") },
        });
    }

    Ok(f(gl, guard.as_ref().unwrap()))
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn paint_wgl_video_texture(ui: &mut egui::Ui, rect: egui::Rect, gl_tex_id: u32) {
    let callback = egui::PaintCallback {
        rect,
        callback: Arc::new(egui_glow::CallbackFn::new(move |_info, painter| {
            use eframe::glow::HasContext;

            let gl = painter.gl();
            let texture = eframe::glow::NativeTexture(
                NonZeroU32::new(gl_tex_id).expect("WGL texture id must be non-zero"),
            );
            if let Err(e) = with_wgl_video_callback_renderer(gl, |gl, renderer| unsafe {
                gl.disable(eframe::glow::BLEND);
                gl.disable(eframe::glow::CULL_FACE);
                gl.disable(eframe::glow::DEPTH_TEST);
                gl.use_program(Some(renderer.program));
                gl.active_texture(eframe::glow::TEXTURE0);
                gl.bind_texture(eframe::glow::TEXTURE_2D, Some(texture));
                gl.bind_vertex_array(Some(renderer.vao));
                gl.bind_buffer(eframe::glow::ARRAY_BUFFER, Some(renderer.vbo));
                if let Some(u_sampler) = renderer.u_sampler.as_ref() {
                    gl.uniform_1_i32(Some(u_sampler), 0);
                }
                gl.draw_arrays(eframe::glow::TRIANGLE_STRIP, 0, 4);
                gl.bind_buffer(eframe::glow::ARRAY_BUFFER, None);
                gl.bind_vertex_array(None);
                gl.bind_texture(eframe::glow::TEXTURE_2D, None);
                gl.use_program(None);
            }) {
                eprintln!("[Phase 3.5] WGL video callback render failed: {e}");
            }
        })),
    };
    ui.painter().add(callback);
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn register_wgl_video_texture(
    gl: &eframe::glow::Context,
    eframe_frame: &mut eframe::Frame,
    width: u32,
    height: u32,
) -> Result<(egui::TextureId, u32), String> {
    use eframe::glow::HasContext;

    let tex =
        unsafe { gl.create_texture() }.map_err(|e| format!("gl.create_texture failed: {e}"))?;
    let raw_id = tex.0.get();
    unsafe {
        gl.bind_texture(eframe::glow::TEXTURE_2D, Some(tex));
        gl.tex_parameter_i32(
            eframe::glow::TEXTURE_2D,
            eframe::glow::TEXTURE_MIN_FILTER,
            eframe::glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            eframe::glow::TEXTURE_2D,
            eframe::glow::TEXTURE_MAG_FILTER,
            eframe::glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            eframe::glow::TEXTURE_2D,
            eframe::glow::TEXTURE_WRAP_S,
            eframe::glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            eframe::glow::TEXTURE_2D,
            eframe::glow::TEXTURE_WRAP_T,
            eframe::glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            eframe::glow::TEXTURE_2D,
            eframe::glow::TEXTURE_BASE_LEVEL,
            0,
        );
        gl.tex_parameter_i32(eframe::glow::TEXTURE_2D, eframe::glow::TEXTURE_MAX_LEVEL, 0);
        // Explicit RGBA8 storage avoids driver-chosen defaults in the WGL interop path.
        gl.tex_image_2d(
            eframe::glow::TEXTURE_2D,
            0,
            eframe::glow::RGBA8 as i32,
            width as i32,
            height as i32,
            0,
            eframe::glow::RGBA,
            eframe::glow::UNSIGNED_BYTE,
            None,
        );
        gl.bind_texture(eframe::glow::TEXTURE_2D, None);
    }

    if crate::telemetry::is_telemetry_enabled() {
        eprintln!(
            "[Phase 3.5] GL video texture allocated: {}x{} {} tex={}",
            width, height, "RGBA8", raw_id
        );
    }
    let egui_tex_id = eframe_frame.register_native_glow_texture(tex);
    Ok((egui_tex_id, raw_id))
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
    engine_done: &mut Option<std::sync::mpsc::Receiver<()>>,
    video_frames: &mut Option<VideoFrames>,
    // Phase 3.5: OpenGL context for GPU texture management (WGL_NV_DX_interop2).
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))] gl_ctx: Option<
        std::sync::Arc<eframe::glow::Context>,
    >,
    // Phase 3.5: WGL_NV_DX_interop2 manager (None = CPU path fallback).
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))] mut gl_interop: Option<
        &mut crate::d3d11_gl_interop::D3d11GlInterop,
    >,
    // Phase 3.5: eframe Frame for register_native_glow_texture().
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))] eframe_frame: &mut eframe::Frame,
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
        let streaming_user_ids: HashSet<i64> = state
            .main
            .voice
            .participants
            .iter()
            .filter(|p| p.streaming)
            .map(|p| p.user_id)
            .collect();

        if let Some(uid) = state.main.fullscreen_stream_user {
            if !streaming_user_ids.contains(&uid) {
                state.main.fullscreen_stream_user = None;
            }
        }

        let preview_keys_to_remove: Vec<i64> = state
            .main
            .voice
            .participants
            .iter()
            .filter(|p| !p.streaming)
            .map(|p| video_preview_frame_key(p.user_id))
            .filter(|key| {
                state.main.voice_video_gpu_textures.contains_key(key)
                    || state.main.voice_video_textures.contains_key(key)
            })
            .collect();

        let stream_keys_non_streaming: Vec<i64> = state
            .main
            .voice_video_gpu_textures
            .keys()
            .chain(state.main.voice_video_textures.keys())
            .filter(|&&k| k < 0 && k > i64::MIN / 2)
            .filter(|&&k| !streaming_user_ids.contains(&(-k - 1)))
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Debounce: only remove if we saw this key as non-streaming in the previous frame too.
        let stream_keys_to_remove: Vec<i64> = stream_keys_non_streaming
            .iter()
            .filter(|k| state.main.stream_ended_prev_frame.contains(k))
            .copied()
            .collect();

        state.main.stream_ended_prev_frame = stream_keys_non_streaming.into_iter().collect();

        if !stream_keys_to_remove.is_empty() {
            for key in &stream_keys_to_remove {
                state.main.stream_ended_prev_frame.remove(key);
                if let Some((egui_tex_id, _, _, _)) =
                    state.main.voice_video_gpu_textures.remove(key)
                {
                    state
                        .main
                        .voice_video_gpu_tex_pending_delete
                        .push(egui_tex_id);
                }
                state.main.voice_video_textures.remove(key);
                state.main.voice_render_fps.remove(key);
                let uid = -key - 1;
                state
                    .main
                    .voice_video_textures
                    .remove(&video_preview_frame_key(uid));
                if let Some((egui_tex_id, _, _, _)) = state
                    .main
                    .voice_video_gpu_textures
                    .remove(&video_preview_frame_key(uid))
                {
                    state
                        .main
                        .voice_video_gpu_tex_pending_delete
                        .push(egui_tex_id);
                }
                if state.main.fullscreen_stream_user == Some(uid) {
                    state.main.fullscreen_stream_user = None;
                }
            }
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            if let Some(interop) = gl_interop.as_deref_mut() {
                interop.remove_keys(&stream_keys_to_remove);
            }
        }
        if !preview_keys_to_remove.is_empty() {
            for key in &preview_keys_to_remove {
                if let Some((egui_tex_id, _, _, _)) =
                    state.main.voice_video_gpu_textures.remove(key)
                {
                    state
                        .main
                        .voice_video_gpu_tex_pending_delete
                        .push(egui_tex_id);
                }
                state.main.voice_video_textures.remove(key);
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
                    // Get or create a GL texture for this stream key.
                    // Tuple: (egui TextureId registered with painter, raw GL name for WGL)
                    let existing_tex = state.main.voice_video_gpu_textures.get(&key).copied();
                    let mut old_tex_to_delete = None;
                    let mut new_tex_to_delete_on_fail = None;
                    let tex_pair = if let Some((eid, gid, old_w, old_h)) = existing_tex {
                        if old_w == frame.width && old_h == frame.height {
                            Some((eid, gid))
                        } else {
                            eprintln!(
                                "[Phase 3.5] GL video texture resize: key={key} {}x{} → {}x{}",
                                old_w, old_h, frame.width, frame.height
                            );
                            match register_wgl_video_texture(
                                gl,
                                eframe_frame,
                                frame.width,
                                frame.height,
                            ) {
                                Ok((new_eid, new_gid)) => {
                                    old_tex_to_delete = Some(eid);
                                    new_tex_to_delete_on_fail = Some(new_eid);
                                    Some((new_eid, new_gid))
                                }
                                Err(e) => {
                                    eprintln!("[Phase 3.5] {e}");
                                    None
                                }
                            }
                        }
                    } else {
                        match register_wgl_video_texture(
                            gl,
                            eframe_frame,
                            frame.width,
                            frame.height,
                        ) {
                            Ok(pair) => {
                                new_tex_to_delete_on_fail = Some(pair.0);
                                Some(pair)
                            }
                            Err(e) => {
                                eprintln!("[Phase 3.5] {e}");
                                None
                            }
                        }
                    };
                    if let Some((egui_tex_id, gl_tex_id)) = tex_pair {
                        // Register (or re-register on handle change) the D3D11 shared texture.
                        match interop.update_texture(
                            key,
                            handle,
                            gl_tex_id,
                            frame.width,
                            frame.height,
                        ) {
                            Ok(()) => {
                                if let Some(tex_id) = old_tex_to_delete.take() {
                                    state.main.voice_video_gpu_tex_pending_delete.push(tex_id);
                                }
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
                                if let Some(tex_id) = new_tex_to_delete_on_fail.take() {
                                    state.main.voice_video_gpu_tex_pending_delete.push(tex_id);
                                }
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
                let should_print = state
                    .main
                    .voice_telemetry_print_at
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
    let is_text_channel = state
        .main
        .channels
        .iter()
        .find(|c| Some(c.id) == state.main.selected_channel)
        .map(|c| c.r#type == "text")
        .unwrap_or(false);

    // ── Dialog: voice server switch confirmation ──────────────────────────
    if let Some((target_ch, target_srv)) = state.main.voice_switch_confirm {
        let mut do_switch = false;
        let mut do_cancel = false;
        let cur_ch_name: String = state
            .main
            .channels
            .iter()
            .find(|c| state.main.voice.channel_id == Some(c.id))
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let cur_srv_name: String = state
            .main
            .servers
            .iter()
            .find(|s| state.main.voice.server_id == Some(s.id))
            .map(|s| s.name.clone())
            .unwrap_or_default();
        egui::Window::new("Переключить голосовой канал?")
            .collapsible(false)
            .resizable(false)
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
                    if ui.button("Переключить").clicked() {
                        do_switch = true;
                    }
                    if ui.button("Отмена").clicked() {
                        do_cancel = true;
                    }
                });
            });
        if do_switch {
            state.main.voice_pending_leave = false;
            // Leave current, then join target
            if let Some(cur_ch) = state.main.voice.channel_id {
                if let Some(ref token) = state.access_token {
                    let token = token.clone();
                    let _ = block_on(api.voice_leave(&token, cur_ch));
                }
            }
            // Stop current engine
            stop_voice_engine(engine_tx, engine_done);
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
                        state
                            .main
                            .channel_voice
                            .insert(target_ch, resp.participants.clone());
                        // Start new engine for the switched channel (LiveKit)
                        let rt = tokio::runtime::Handle::current();
                        let (tx, vf, done) = spawn_voice_engine(rt);
                        *video_frames = Some(vf);
                        let receiver_telemetry = Arc::new(PipelineTelemetry::new());
                        state.main.voice_receiver_telemetry = Some(Arc::clone(&receiver_telemetry));
                        let session_stats = state
                            .main
                            .voice_stats
                            .get_or_insert_with(|| {
                                Arc::new(Mutex::new(VoiceSessionStats::default()))
                            })
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
                        })
                        .ok();
                        tx.send(VoiceCmd::SetInputVolume(state.main.voice.input_volume))
                            .ok();
                        tx.send(VoiceCmd::SetOutputVolume(state.main.voice.output_volume))
                            .ok();
                        for participant in &resp.participants {
                            let denoise_enabled = state
                                .settings
                                .receiver_denoise_by_user
                                .contains(&participant.user_id.to_string());
                            if denoise_enabled {
                                state
                                    .main
                                    .voice
                                    .receiver_denoise_users
                                    .insert(participant.user_id);
                            } else {
                                state
                                    .main
                                    .voice
                                    .receiver_denoise_users
                                    .remove(&participant.user_id);
                            }
                            tx.send(VoiceCmd::SetRemoteVoiceDenoise {
                                user_id: participant.user_id,
                                enabled: denoise_enabled,
                            })
                            .ok();
                        }
                        *engine_tx = Some(tx);
                        *engine_done = Some(done);
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
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Название сервера:");
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut state.main.new_server_name)
                        .hint_text("Мой сервер")
                        .desired_width(220.0),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Создать").clicked() {
                        should_create = true;
                    }
                    if ui.button("Отмена").clicked() {
                        should_cancel = true;
                    }
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
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Название канала:");
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut state.main.new_channel_name)
                        .hint_text("общий")
                        .desired_width(220.0),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.radio_value(&mut state.main.new_channel_is_voice, false, "Текстовый");
                    ui.radio_value(&mut state.main.new_channel_is_voice, true, "Голосовой");
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Создать").clicked() {
                        should_create = true;
                    }
                    if ui.button("Отмена").clicked() {
                        should_cancel = true;
                    }
                });
            });
        if should_create {
            let name = state.main.new_channel_name.trim().to_string();
            let ch_type = if state.main.new_channel_is_voice {
                "voice"
            } else {
                "text"
            };
            if !name.is_empty() {
                if let (Some(server_id), Some(ref token)) =
                    (state.main.selected_server, &state.access_token)
                {
                    let token = token.clone();
                    if let Ok(ch) = block_on(api.create_channel(&token, server_id, &name, ch_type))
                    {
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
        let srv_label: String = state
            .main
            .servers
            .iter()
            .find(|s| Some(s.id) == state.main.selected_server)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        egui::Window::new(format!("Пригласить на «{}»", srv_label))
            .collapsible(false)
            .resizable(false)
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
                ui.add(
                    egui::TextEdit::singleline(&mut state.main.invite_user_id_input)
                        .hint_text("Числовой ID")
                        .desired_width(220.0),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Пригласить").clicked() {
                        should_invite = true;
                    }
                    if ui.button("Закрыть").clicked() {
                        should_close = true;
                    }
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
                            state.main.invite_msg =
                                Some("Ошибка: не найден или уже в сервере.".to_string());
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
        let mut new_input_vol: Option<f32> = None;
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
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Модель шумоподавления DeepFilterNet:").small());
                ui.label(
                    egui::RichText::new(
                        "Доступны только модели, проверенные с текущим Windows libDF runtime."
                    )
                        .small()
                        .weak(),
                );
                let mut denoise_model_id = state.settings.denoise_model_id.clone();
                let selected_model_text = if crate::denoise::is_model_downloaded(&denoise_model_id) {
                    crate::denoise::model_label(&denoise_model_id).to_string()
                } else {
                    format!(
                        "{} (не скачана)",
                        crate::denoise::model_label(&denoise_model_id)
                    )
                };
                egui::ComboBox::from_id_source("denoise_model_id")
                    .selected_text(selected_model_text)
                    .show_ui(ui, |ui| {
                        for model in crate::denoise::known_models() {
                            let label = if crate::denoise::is_model_downloaded(model.id) {
                                model.label.to_string()
                            } else {
                                format!("{} (не скачана)", model.label)
                            };
                            let _ = ui.selectable_value(
                                &mut denoise_model_id,
                                model.id.to_string(),
                                label,
                            );
                        }
                    });
                if denoise_model_id != state.settings.denoise_model_id {
                    state.settings.denoise_model_id = denoise_model_id.clone();
                    crate::denoise::set_selected_model(&denoise_model_id);
                    state.settings.save();
                }
                ui.label(
                    egui::RichText::new(format!(
                        "Файл: {}",
                        if state.settings.denoise_model_id == "off" {
                            "—".to_string()
                        } else {
                            state.settings.denoise_model_id.clone()
                        }
                    ))
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                if state.settings.denoise_model_id == "off" {
                    ui.label(
                        egui::RichText::new("Шумоподавление полностью отключено.")
                            .small()
                            .weak(),
                    );
                } else {
                    ui.label(egui::RichText::new("Режим маски:").small());
                    let mut denoise_reduce_mask = state.settings.denoise_reduce_mask.clone();
                    egui::ComboBox::from_id_source("denoise_reduce_mask")
                        .selected_text(
                            crate::denoise::reduce_mask_label(&denoise_reduce_mask).to_string(),
                        )
                        .show_ui(ui, |ui| {
                            for mask in crate::denoise::known_reduce_masks() {
                                let _ = ui.selectable_value(
                                    &mut denoise_reduce_mask,
                                    mask.id.to_string(),
                                    mask.label,
                                );
                            }
                        });
                    if denoise_reduce_mask != state.settings.denoise_reduce_mask {
                        state.settings.denoise_reduce_mask = denoise_reduce_mask.clone();
                        crate::denoise::set_denoise_reduce_mask(&denoise_reduce_mask);
                        state.settings.save();
                    }
                    ui.label(
                        egui::RichText::new(
                            "Для mono voice-трактов этот режим влияет слабее, чем остальные параметры."
                        )
                        .small()
                        .weak(),
                    );
                    ui.add_space(4.0);

                    ui.label(egui::RichText::new("Лимит подавления:").small());
                    let mut denoise_atten_lim_db = state.settings.denoise_atten_lim_db;
                    let atten_slider =
                        egui::Slider::new(&mut denoise_atten_lim_db, 0.0_f32..=80.0_f32)
                            .custom_formatter(|v, _| format!("{v:.0} dB"))
                            .show_value(true);
                    if ui.add(atten_slider).changed() {
                        denoise_atten_lim_db =
                            crate::denoise::normalize_atten_lim_db(denoise_atten_lim_db);
                        state.settings.denoise_atten_lim_db = denoise_atten_lim_db;
                        crate::denoise::set_denoise_atten_lim_db(denoise_atten_lim_db);
                        state.settings.save();
                    }

                    ui.label(egui::RichText::new("Post-filter beta:").small());
                    let mut denoise_post_filter_beta = state.settings.denoise_post_filter_beta;
                    let post_slider =
                        egui::Slider::new(&mut denoise_post_filter_beta, 0.0_f32..=0.050_f32)
                            .step_by(0.001)
                            .custom_formatter(|v, _| format!("{v:.3}"))
                            .show_value(true);
                    if ui.add(post_slider).changed() {
                        denoise_post_filter_beta =
                            crate::denoise::normalize_post_filter_beta(denoise_post_filter_beta);
                        state.settings.denoise_post_filter_beta = denoise_post_filter_beta;
                        crate::denoise::set_denoise_post_filter_beta(denoise_post_filter_beta);
                        state.settings.save();
                    }

                    ui.label(egui::RichText::new("Min dB threshold:").small());
                    let mut denoise_min_db_thresh = state.settings.denoise_min_db_thresh;
                    let min_thresh_slider =
                        egui::Slider::new(&mut denoise_min_db_thresh, -40.0_f32..=5.0_f32)
                            .custom_formatter(|v, _| format!("{v:.0} dB"))
                            .show_value(true);
                    if ui.add(min_thresh_slider).changed() {
                        denoise_min_db_thresh =
                            crate::denoise::normalize_min_db_thresh(denoise_min_db_thresh);
                        state.settings.denoise_min_db_thresh = denoise_min_db_thresh;
                        crate::denoise::set_denoise_thresholds(
                            denoise_min_db_thresh,
                            state.settings.denoise_max_db_erb_thresh,
                            state.settings.denoise_max_db_df_thresh,
                        );
                        state.settings.save();
                    }

                    ui.label(egui::RichText::new("Max ERB threshold:").small());
                    let mut denoise_max_db_erb_thresh = state.settings.denoise_max_db_erb_thresh;
                    let max_erb_slider =
                        egui::Slider::new(&mut denoise_max_db_erb_thresh, 0.0_f32..=60.0_f32)
                            .custom_formatter(|v, _| format!("{v:.0} dB"))
                            .show_value(true);
                    if ui.add(max_erb_slider).changed() {
                        denoise_max_db_erb_thresh = crate::denoise::normalize_max_db_erb_thresh(
                            denoise_max_db_erb_thresh,
                        );
                        state.settings.denoise_max_db_erb_thresh = denoise_max_db_erb_thresh;
                        crate::denoise::set_denoise_thresholds(
                            state.settings.denoise_min_db_thresh,
                            denoise_max_db_erb_thresh,
                            state.settings.denoise_max_db_df_thresh,
                        );
                        state.settings.save();
                    }

                    ui.label(egui::RichText::new("Max DF threshold:").small());
                    let mut denoise_max_db_df_thresh = state.settings.denoise_max_db_df_thresh;
                    let max_df_slider =
                        egui::Slider::new(&mut denoise_max_db_df_thresh, 0.0_f32..=60.0_f32)
                            .custom_formatter(|v, _| format!("{v:.0} dB"))
                            .show_value(true);
                    if ui.add(max_df_slider).changed() {
                        denoise_max_db_df_thresh =
                            crate::denoise::normalize_max_db_df_thresh(denoise_max_db_df_thresh);
                        state.settings.denoise_max_db_df_thresh = denoise_max_db_df_thresh;
                        crate::denoise::set_denoise_thresholds(
                            state.settings.denoise_min_db_thresh,
                            state.settings.denoise_max_db_erb_thresh,
                            denoise_max_db_df_thresh,
                        );
                        state.settings.save();
                    }

                    if ui.button("Сбросить параметры шумодава").clicked() {
                        state.settings.denoise_atten_lim_db =
                            crate::denoise::default_atten_lim_db();
                        state.settings.denoise_post_filter_beta =
                            crate::denoise::default_post_filter_beta();
                        state.settings.denoise_min_db_thresh =
                            crate::denoise::default_min_db_thresh();
                        state.settings.denoise_max_db_erb_thresh =
                            crate::denoise::default_max_db_erb_thresh();
                        state.settings.denoise_max_db_df_thresh =
                            crate::denoise::default_max_db_df_thresh();
                        state.settings.denoise_reduce_mask =
                            crate::denoise::default_reduce_mask_id().to_string();
                        crate::denoise::set_denoise_reduce_mask(&state.settings.denoise_reduce_mask);
                        crate::denoise::set_denoise_atten_lim_db(
                            state.settings.denoise_atten_lim_db,
                        );
                        crate::denoise::set_denoise_post_filter_beta(
                            state.settings.denoise_post_filter_beta,
                        );
                        crate::denoise::set_denoise_thresholds(
                            state.settings.denoise_min_db_thresh,
                            state.settings.denoise_max_db_erb_thresh,
                            state.settings.denoise_max_db_df_thresh,
                        );
                        state.settings.save();
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "Текущие параметры: atten {:.0} dB, beta {:.3}, thresholds [{:.0}, {:.0}, {:.0}], mask {}",
                            state.settings.denoise_atten_lim_db,
                            state.settings.denoise_post_filter_beta,
                            state.settings.denoise_min_db_thresh,
                            state.settings.denoise_max_db_erb_thresh,
                            state.settings.denoise_max_db_df_thresh,
                            crate::denoise::reduce_mask_label(&state.settings.denoise_reduce_mask)
                        ))
                        .small()
                        .weak(),
                    );
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
                ui.label(egui::RichText::new("Legacy note: after the color-path fix this should normally stay at 0. Non-zero values intentionally darken the image.").small());
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
                    Err(e) => {
                        state.main.settings_msg = Some(format!("Ошибка: {e}"));
                    }
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
                            Ok(_) => {
                                state.main.settings_msg = Some("Аватарка обновлена.".to_string());
                            }
                            Err(e) => {
                                state.main.settings_msg = Some(format!("Ошибка: {e}"));
                            }
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
        let ch_name_orig = state
            .main
            .channels
            .iter()
            .find(|c| c.id == ch_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let mut should_save = false;
        let mut should_cancel = false;
        let mut new_name = rename_input.clone();
        egui::Window::new(format!("Переименовать «{}»", ch_name_orig))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut new_name).desired_width(220.0));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Сохранить").clicked() {
                        should_save = true;
                    }
                    if ui.button("Отмена").clicked() {
                        should_cancel = true;
                    }
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
                            if c.id == ch.id {
                                c.name = ch.name;
                                break;
                            }
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
        .exact_height(1.0)
        .show_separator_line(false)
        .show(ctx, |ui| {
            // Phase 3.5: sRGB diagnostic — log GL_FRAMEBUFFER_SRGB state when GPU video active.
            // If ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB=1, also disable it (test for double sRGB).
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            if !state.main.voice_video_gpu_textures.is_empty() {
                let disable_fb_srgb = std::env::var("ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB")
                    .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                    .unwrap_or(true);
                static LOGGED_SRGB: AtomicBool = AtomicBool::new(false);
                let rect = ui.available_rect_before_wrap();
                let callback = egui::PaintCallback {
                    rect,
                    callback: Arc::new(egui_glow::CallbackFn::new(move |_info, painter| {
                        let gl = painter.gl();
                        unsafe {
                            use eframe::glow::HasContext;
                            let was_enabled = gl.is_enabled(eframe::glow::FRAMEBUFFER_SRGB);
                            if crate::telemetry::is_telemetry_enabled()
                                && !LOGGED_SRGB.swap(true, Ordering::Relaxed)
                            {
                                eprintln!(
                                    "[Phase 3.5] GL_FRAMEBUFFER_SRGB was {} (video path {} it by default; override with ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB=0)",
                                    if was_enabled { "enabled" } else { "disabled" },
                                    if disable_fb_srgb { "disables" } else { "keeps" }
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
    let user_display = if state.main.my_display_name.is_empty() {
        state.auth.username.clone()
    } else {
        state.main.my_display_name.clone()
    };
    let left_user_panel_height = bottom_panel::panel_height(state.main.voice.channel_id.is_some());

    egui::SidePanel::left("panel_servers")
        .frame(egui::Frame::none().fill(theme.bg_tertiary))
        .exact_width(guild_panel::GUILD_PANEL_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .show(ctx, |ui| {
            let selected_server = state.main.selected_server;
            let servers = state.main.servers.clone();
            let mut on_action = |act: guild_panel::GuildPanelAction| match act {
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
                    todo_actions::todo_explore_servers();
                }
                guild_panel::GuildPanelAction::DeleteServer(id) => {
                    state.main.server_to_delete = Some(id);
                }
                guild_panel::GuildPanelAction::RetryServers => {
                    state.main.retry_servers = true;
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
                    bottom_reserved_height: left_user_panel_height,
                    servers: servers.as_slice(),
                    selected_server,
                    on_action: &mut on_action,
                    servers_loading,
                    servers_error,
                },
            );
        });

    // ── Left panel: channels list (channel_panel) ───────────────────────────
    let mut should_logout = false;
    if server_selected {
        let text_chs: Vec<(i64, String)> = state
            .main
            .channels
            .iter()
            .filter(|c| c.r#type == "text")
            .map(|c| (c.id, c.name.clone()))
            .collect();
        let voice_chs: Vec<(i64, String)> = state
            .main
            .channels
            .iter()
            .filter(|c| c.r#type == "voice")
            .map(|c| (c.id, c.name.clone()))
            .collect();
        let voice_snapshot = ChannelPanelVoiceSnapshot {
            channel_id: state.main.voice.channel_id,
            server_id: state.main.voice.server_id,
            mic_muted: state.main.voice.mic_muted,
            output_muted: state.main.voice.output_muted,
            camera_on: state.main.voice.camera_on,
            screen_on: state.main.voice.screen_on,
            channel_voice: state.main.channel_voice.clone(),
            speaking: state.main.voice.speaking.lock().clone(),
            local_volumes: state.main.voice.local_volumes.clone(),
            locally_muted: state.main.voice.locally_muted.clone(),
            receiver_denoise_users: state.main.voice.receiver_denoise_users.clone(),
        };
        let server_name = state
            .main
            .servers
            .iter()
            .find(|s| Some(s.id) == state.main.selected_server)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let server_id = state.main.selected_server.unwrap_or(0);
        let mut channel_actions: Vec<ChannelPanelAction> = Vec::new();
        egui::SidePanel::left("panel_channels")
            .frame(egui::Frame::none().fill(theme.bg_secondary))
            .exact_width(channel_panel::CHANNEL_PANEL_WIDTH)
            .resizable(false)
            .show_separator_line(false)
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
                        bottom_reserved_height: left_user_panel_height,
                        server_name: &server_name,
                        server_id,
                        text_channels: &text_chs,
                        voice_channels: &voice_chs,
                        unread_channel_ids: &state.main.unread_channels,
                        selected_channel_id: state.main.selected_channel,
                        voice: voice_snapshot.clone(),
                        user_id: state.user_id,
                        on_action: &mut |a| channel_actions.push(a.clone()),
                        channels_load,
                    },
                );
            });

        let user_id = state.user_id;
        for act in channel_actions {
            match act {
                ChannelPanelAction::SelectChannel(id) => {
                    state.main.selected_channel = Some(id);
                    state.main.unread_channels.remove(&id);
                }
                ChannelPanelAction::JoinVoice {
                    channel_id,
                    server_id: srv_id,
                } => {
                    if state.main.voice.channel_id == Some(channel_id) {
                        // already in this channel
                    } else if state.main.voice.channel_id.is_none() {
                        apply_voice_join(
                            ctx,
                            state,
                            api,
                            engine_tx,
                            engine_done,
                            video_frames,
                            channel_id,
                            srv_id,
                            user_id,
                        );
                    } else if state.main.voice.server_id == Some(srv_id) {
                        apply_voice_join(
                            ctx,
                            state,
                            api,
                            engine_tx,
                            engine_done,
                            video_frames,
                            channel_id,
                            srv_id,
                            user_id,
                        );
                    } else {
                        state.main.voice_switch_confirm = Some((channel_id, srv_id));
                    }
                }
                ChannelPanelAction::LeaveVoice => {
                    apply_voice_leave(ctx, state, api, engine_tx, engine_done, video_frames);
                }
                ChannelPanelAction::SetMicMuted(muted) => {
                    set_local_mic_muted(ctx, state, api, engine_tx.as_ref(), user_id, muted);
                }
                ChannelPanelAction::SetOutputMuted(muted) => {
                    set_local_output_muted(ctx, state, engine_tx.as_ref(), muted);
                }
                ChannelPanelAction::SetParticipantMuted { user_id, muted } => {
                    if muted {
                        state.main.voice.locally_muted.insert(user_id);
                    } else {
                        state.main.voice.locally_muted.remove(&user_id);
                    }
                    sync_user_volume(engine_tx, &state.main.voice, user_id);
                    ctx.request_repaint();
                }
                ChannelPanelAction::SetParticipantVolume { user_id, volume } => {
                    let volume = volume.clamp(0.0, 3.0);
                    state.main.voice.local_volumes.insert(user_id, volume);
                    state
                        .settings
                        .voice_volume_by_user
                        .insert(user_id.to_string(), volume);
                    state.settings.save();
                    sync_user_volume(engine_tx, &state.main.voice, user_id);
                    ctx.request_repaint();
                }
                ChannelPanelAction::SetParticipantDenoise { user_id, enabled } => {
                    set_receiver_denoise_enabled(state, engine_tx, user_id, enabled);
                    ctx.request_repaint();
                }
                ChannelPanelAction::SetCameraEnabled(enabled) => {
                    set_local_camera_enabled(ctx, state, api, engine_tx.as_ref(), user_id, enabled);
                }
                ChannelPanelAction::ToggleScreenShare => {
                    toggle_local_screen_share(ctx, state, api, engine_tx.as_ref(), user_id);
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
            apply_voice_leave(ctx, state, api, engine_tx, engine_done, video_frames);
        }
    }

    // ── Right panel: members (member_panel) ──────────────────────────────────
    if server_selected {
        let left_user_panel_width =
            guild_panel::GUILD_PANEL_WIDTH + channel_panel::CHANNEL_PANEL_WIDTH;
        let voice_bar_snapshot = BottomPanelVoiceSnapshot {
            in_voice_channel: state.main.voice.channel_id.is_some(),
            mic_muted: state.main.voice.mic_muted,
            output_muted: state.main.voice.output_muted,
            screen_on: state.main.voice.screen_on,
            screen_preset: state.main.screen_preset,
        };
        let mut bottom_actions: Vec<BottomPanelAction> = Vec::new();
        let screen_rect = ctx.screen_rect();

        egui::Area::new(egui::Id::new("left_user_panel"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                screen_rect.left(),
                screen_rect.bottom() - left_user_panel_height,
            ))
            .show(ctx, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(left_user_panel_width, left_user_panel_height),
                    egui::Layout::left_to_right(egui::Align::Min),
                    |ui| {
                        bottom_panel::show(
                            ui,
                            BottomPanelParams {
                                theme,
                                user_display: &user_display,
                                voice: voice_bar_snapshot,
                                avatar_texture: state
                                    .user_id
                                    .and_then(|id| avatar_textures.get(&id)),
                                on_action: &mut |action| bottom_actions.push(action),
                            },
                        );
                    },
                );
            });

        for action in bottom_actions {
            match action {
                BottomPanelAction::SetMicMuted(muted) => {
                    set_local_mic_muted(ctx, state, api, engine_tx.as_ref(), state.user_id, muted);
                }
                BottomPanelAction::SetDeafened(deafened) => {
                    set_local_deafened(
                        ctx,
                        state,
                        api,
                        engine_tx.as_ref(),
                        state.user_id,
                        deafened,
                    );
                }
                BottomPanelAction::OpenSettings => {
                    state.main.show_settings_dialog = true;
                    state.main.settings_nickname_input = state.main.my_display_name.clone();
                    state.main.settings_msg = None;
                }
                BottomPanelAction::OpenStreamPicker => {
                    open_screen_share_picker(ctx, state, true);
                }
                BottomPanelAction::StopStream => {
                    stop_local_screen_share(ctx, state, api, engine_tx.as_ref(), state.user_id);
                }
                BottomPanelAction::SetScreenPreset(preset) => {
                    state.main.screen_preset = preset;
                    ctx.request_repaint();
                }
                BottomPanelAction::LeaveVoice => {
                    apply_voice_leave(ctx, state, api, engine_tx, engine_done, video_frames);
                }
            }
        }
    }

    let show_members = state.main.show_member_panel.unwrap_or(true);
    if server_selected && show_members {
        let online_count = state.main.online_users.len();
        let server_owner_id = state
            .main
            .servers
            .iter()
            .find(|s| Some(s.id) == state.main.selected_server)
            .map(|s| s.owner_id)
            .unwrap_or(0);
        let mut members_snap: Vec<MemberSnapshot> = state
            .main
            .server_members
            .iter()
            .map(|m| {
                let online = state.main.online_users.contains(&m.user_id);
                let is_owner = m.is_owner || m.user_id == server_owner_id;
                let display = if m.display_name.is_empty() {
                    m.username.clone()
                } else {
                    m.display_name.clone()
                };
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
        let mut member_actions: Vec<MemberPanelAction> = Vec::new();

        egui::SidePanel::right("panel_members")
            .frame(egui::Frame::none().fill(theme.bg_secondary))
            .exact_width(member_panel::MEMBER_PANEL_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .show(ctx, |ui| {
                member_panel::show(
                    ctx,
                    ui,
                    MemberPanelParams {
                        theme,
                        members: &members_snap,
                        online_count,
                        speaking: &speaking_snap,
                        avatar_textures,
                        on_action: &mut |action| member_actions.push(action),
                    },
                );
            });

        for action in member_actions {
            match action {
                MemberPanelAction::OpenMemberProfile(user_id) => {
                    todo_actions::todo_open_member_profile(user_id);
                }
            }
        }
    }

    // ── Central panel: chat ───────────────────────────────────────────────
    egui::CentralPanel::default()
        .frame(egui::Frame::none().fill(theme.bg_primary))
        .show(ctx, |ui| {
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
                avatar_textures,
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
                    ChatPanelAction::Threads => todo_actions::todo_open_threads(),
                    ChatPanelAction::Notifications => {
                        todo_actions::todo_open_notifications()
                    }
                    ChatPanelAction::Pinned => todo_actions::todo_open_pins(),
                    ChatPanelAction::Search => todo_actions::todo_search_messages(),
                    ChatPanelAction::Inbox => todo_actions::todo_open_inbox(),
                    ChatPanelAction::Help => todo_actions::todo_open_help(),
                    ChatPanelAction::ToggleMemberList => {
                        let current = state.main.show_member_panel.unwrap_or(true);
                        state.main.show_member_panel = Some(!current);
                        ctx.request_repaint();
                    }
                    ChatPanelAction::StubGif => todo_actions::todo_insert_gif(),
                    ChatPanelAction::StubEmoji => todo_actions::todo_open_emoji_picker(),
                    ChatPanelAction::StubStickers => {
                        todo_actions::todo_open_sticker_picker()
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
                                        let is_stream_subscribed = in_this_voice
                                            && state.main.voice.stream_subscriptions.contains(&p.user_id);
                                        let show_stream_preview = p.streaming && in_this_voice && !is_stream_subscribed;

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
                                        let stream_preview_key = video_preview_frame_key(p.user_id);
                                        let camera_key = p.user_id;
                                        let has_stream_texture =
                                            state.main.voice_video_gpu_textures.contains_key(&stream_key)
                                                || state.main.voice_video_textures.contains_key(&stream_key);
                                        let has_stream_preview_texture =
                                            state.main.voice_video_gpu_textures.contains_key(&stream_preview_key)
                                                || state.main.voice_video_textures.contains_key(&stream_preview_key);
                                        let has_camera_texture =
                                            state.main.voice_video_gpu_textures.contains_key(&camera_key)
                                                || state.main.voice_video_textures.contains_key(&camera_key);
                                        let show_stream_connecting =
                                            p.streaming && in_this_voice && is_stream_subscribed && !has_stream_texture;
                                        let show_stream_controls =
                                            p.streaming && in_this_voice && is_stream_subscribed && has_stream_texture;
                                        let tex_key = if p.streaming {
                                            if has_stream_texture {
                                                Some(stream_key)
                                            } else if has_stream_preview_texture && !is_stream_subscribed {
                                                Some(stream_preview_key)
                                            } else {
                                                None
                                            }
                                        } else if has_camera_texture {
                                            Some(camera_key)
                                        } else {
                                            None
                                        };
                                        if let Some(key) = tex_key {
                                            // Phase 3.5: prefer GPU zero-copy texture (TextureId::User),
                                            // fall back to CPU-uploaded TextureHandle.
                                            let rendered = if let Some(&(_, gl_tex_id, _, _)) = state.main.voice_video_gpu_textures.get(&key) {
                                                paint_wgl_video_texture(ui, avatar_rect, gl_tex_id);
                                                true
                                            } else {
                                                false
                                            };
                                            if !rendered {
                                                if let Some(tex) = state.main.voice_video_textures.get(&key) {
                                                    let size = avatar_rect.size();
                                                    ui.put(avatar_rect, egui::Image::new(tex).fit_to_exact_size(size));
                                                }
                                            }
                                                if is_speaking && !p.streaming {
                                                    ui.painter().rect_stroke(
                                                        avatar_rect.expand(2.0),
                                                        egui::Rounding::same(ROUNDING + 2.0),
                                                        egui::Stroke::new(2.0, egui::Color32::from_rgb(67, 181, 129)),
                                                    );
                                                }
                                                if show_stream_preview {
                                                    ui.painter().rect_filled(
                                                        avatar_rect,
                                                        egui::Rounding::same(ROUNDING),
                                                        egui::Color32::from_black_alpha(120),
                                                    );
                                                    let watch_rect = egui::Rect::from_center_size(
                                                        avatar_rect.center(),
                                                        egui::vec2(120.0, 34.0),
                                                    );
                                                    ui.allocate_ui_at_rect(watch_rect, |ui| {
                                                        if ui.button("Смотреть").clicked() {
                                                            set_stream_subscription(state, engine_tx, p.user_id, true);
                                                        }
                                                    });
                                                }
                                                if show_stream_connecting {
                                                    ui.painter().rect_filled(
                                                        avatar_rect,
                                                        egui::Rounding::same(ROUNDING),
                                                        egui::Color32::from_black_alpha(96),
                                                    );
                                                    ui.painter().text(
                                                        avatar_rect.center(),
                                                        egui::Align2::CENTER_CENTER,
                                                        "Подключение...",
                                                        egui::FontId::proportional(16.0),
                                                        egui::Color32::WHITE,
                                                    );
                                                }
                                                // Stream tile: overlay with fullscreen + mute in corner
                                                if show_stream_controls {
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
                                                        show_stream_audio_button(ui, state, engine_tx, p.user_id);
                                                    });
                                                }
                                                // FPS overlay: отрисованные кадры/сек (не полученные/декодированные)
                                                if show_stream_controls {
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
                                            if show_stream_preview {
                                                ui.painter().rect_filled(
                                                    avatar_rect,
                                                    egui::Rounding::same(ROUNDING),
                                                    egui::Color32::from_black_alpha(120),
                                                );
                                                let watch_rect = egui::Rect::from_center_size(
                                                    avatar_rect.center(),
                                                    egui::vec2(120.0, 34.0),
                                                );
                                                ui.allocate_ui_at_rect(watch_rect, |ui| {
                                                    if ui.button("Смотреть").clicked() {
                                                        set_stream_subscription(state, engine_tx, p.user_id, true);
                                                    }
                                                });
                                            }
                                            if show_stream_connecting {
                                                ui.painter().rect_filled(
                                                    avatar_rect,
                                                    egui::Rounding::same(ROUNDING),
                                                    egui::Color32::from_black_alpha(96),
                                                );
                                                ui.painter().text(
                                                    avatar_rect.center(),
                                                    egui::Align2::CENTER_CENTER,
                                                    "Подключение...",
                                                    egui::FontId::proportional(16.0),
                                                    egui::Color32::WHITE,
                                                );
                                            }
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
                                                    } else {
                                                        state.main.voice.locally_muted.insert(p.user_id);
                                                    }
                                                    sync_user_volume(engine_tx, &state.main.voice, p.user_id);
                                                    ui.close_menu();
                                                }
                                                let denoise_enabled = state
                                                    .main
                                                    .voice
                                                    .receiver_denoise_users
                                                    .contains(&p.user_id);
                                                let denoise_label = if denoise_enabled {
                                                    "Выключить шумоподавление (локально)"
                                                } else {
                                                    "Включить шумоподавление (локально)"
                                                };
                                                if ui.button(denoise_label).clicked() {
                                                    set_receiver_denoise_enabled(
                                                        state,
                                                        engine_tx,
                                                        p.user_id,
                                                        !denoise_enabled,
                                                    );
                                                    ui.close_menu();
                                                }
                                                ui.label("Громкость 0–300%, по умолчанию 100%");
                                                let uid = p.user_id;
                                                let mut vol = *state.main.voice.local_volumes.get(&uid).unwrap_or(&1.0);
                                                if ui.add(egui::Slider::new(&mut vol, 0.0..=3.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)).text("")).changed() {
                                                    state.main.voice.local_volumes.insert(uid, vol);
                                                    state.settings.voice_volume_by_user.insert(uid.to_string(), vol);
                                                    state.settings.save();
                                                    sync_user_volume(engine_tx, &state.main.voice, uid);
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
                        if state.main.voice.screen_on {
                            if ui.button("📺 Остановить трансляцию").on_hover_text("Остановить демонстрацию").clicked() {
                                stop_local_screen_share(
                                    ui.ctx(),
                                    state,
                                    api,
                                    engine_tx.as_ref(),
                                    state.user_id,
                                );
                            }
                        } else {
                            if ui.button("🖥 Выбрать источник").on_hover_text("Выбрать экран или окно для трансляции").clicked() {
                                open_screen_share_picker(ui.ctx(), state, false);
                            }
                            let start_clicked = ui
                                .add_enabled(
                                    state.main.selected_stream_source.is_some(),
                                    egui::Button::new("📺 Начать трансляцию"),
                                )
                                .on_hover_text(if state.main.selected_stream_source.is_some() {
                                    "Запустить трансляцию с выбранного источника"
                                } else {
                                    "Сначала выберите источник"
                                })
                                .clicked();
                            if start_clicked {
                                start_screen_share_from_selected_source(
                                    ui.ctx(),
                                    state,
                                    api,
                                    engine_tx.as_ref(),
                                    state.user_id,
                                );
                            }
                        }
                        if let Some(source) = state.main.selected_stream_source.as_ref() {
                            ui.label(egui::RichText::new(format!("Источник: {}", source.label)).small())
                                .on_hover_text(&source.label);
                        }
                        let stream_audio_label = if state.main.voice.screen_audio_muted {
                            "🔇 Звук стрима"
                        } else {
                            "🔊 Звук стрима"
                        };
                        if ui
                            .button(stream_audio_label)
                            .on_hover_text("Включить или выключить звук, который передается вместе с трансляцией")
                            .clicked()
                        {
                            state.main.voice.screen_audio_muted = !state.main.voice.screen_audio_muted;
                            if let Some(tx) = engine_tx.as_ref() {
                                tx.send(VoiceCmd::SetScreenAudioMuted(state.main.voice.screen_audio_muted)).ok();
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
        let stats = state
            .main
            .voice_stats
            .as_ref()
            .map(|s| s.lock().clone())
            .unwrap_or_default();
        egui::Window::new("Статистика")
            .collapsible(false)
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                let fmt_rtt = stats
                    .latency_rtt_ms
                    .map(|x| format!("{:.0}", x))
                    .unwrap_or_else(|| "—".to_string());
                let fmt_fps = stats
                    .stream_fps
                    .map(|x| format!("{:.1}", x))
                    .unwrap_or_else(|| "—".to_string());
                let fmt_res = stats
                    .resolution
                    .map(|(w, h)| format!("{}×{}", w, h))
                    .unwrap_or_else(|| "—".to_string());
                let fmt_fps2 = stats
                    .frames_per_second
                    .map(|x| format!("{:.1}", x))
                    .unwrap_or_else(|| "—".to_string());
                let fmt_mbps = stats
                    .connection_speed_mbps
                    .map(|x| format!("{:.2}", x))
                    .unwrap_or_else(|| "—".to_string());
                let fmt_in_mbps = stats
                    .incoming_speed_mbps
                    .map(|x| format!("{:.2}", x))
                    .unwrap_or_else(|| "—".to_string());
                let fmt_enc = stats.encoding_path.as_deref().unwrap_or("—");
                let fmt_dec = stats.decoding_path.as_deref().unwrap_or("—");
                let fmt_threads = stats
                    .encoder_threads
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "—".to_string());
                let fmt_dec_threads = stats
                    .decoder_threads
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "—".to_string());
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
        let mut picked_source: Option<ScreenSourceEntry> = None;
        let start_after_pick = state.main.start_stream_after_source_pick;
        let screen_sources = state.main.screen_sources.clone();
        let window_sources = state.main.window_sources.clone();
        let selected_source = state.main.selected_stream_source.clone();
        egui::Window::new("Выбор источника трансляции")
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.heading("Выбор источника трансляции");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            state.main.screen_source_tab == ScreenSourceTab::Applications,
                            "Приложения",
                        )
                        .clicked()
                    {
                        state.main.screen_source_tab = ScreenSourceTab::Applications;
                    }
                    if ui
                        .selectable_label(
                            state.main.screen_source_tab == ScreenSourceTab::EntireScreen,
                            "Весь экран",
                        )
                        .clicked()
                    {
                        state.main.screen_source_tab = ScreenSourceTab::EntireScreen;
                    }
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                match state.main.screen_source_tab {
                    ScreenSourceTab::Applications => {
                        if window_sources.is_empty() {
                            ui.label("Подходящие окна не найдены.");
                        } else {
                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    for source in &window_sources {
                                        let is_selected = selected_source
                                            .as_ref()
                                            .map(|entry| entry.target == source.target)
                                            .unwrap_or(false);
                                        if ui.selectable_label(is_selected, &source.label).clicked() {
                                            picked_source = Some(source.clone());
                                            close_picker = true;
                                        }
                                        ui.add_space(4.0);
                                    }
                                });
                        }
                    }
                    ScreenSourceTab::EntireScreen => {
                        if screen_sources.is_empty() {
                            ui.label("Экраны не обнаружены.");
                        } else {
                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    for source in &screen_sources {
                                        let is_selected = selected_source
                                            .as_ref()
                                            .map(|entry| entry.target == source.target)
                                            .unwrap_or(false);
                                        if ui.selectable_label(is_selected, &source.label).clicked() {
                                            picked_source = Some(source.clone());
                                            close_picker = true;
                                        }
                                        ui.add_space(4.0);
                                    }
                                });
                        }
                    }
                }
                ui.add_space(8.0);
                if !start_after_pick {
                    ui.label("Выбор источника не запускает стрим. Для запуска используйте кнопку \"Начать трансляцию\".");
                    ui.add_space(8.0);
                }
                if ui.button("Отмена").clicked() {
                    close_picker = true;
                }
            });
        if let Some(source) = picked_source {
            state.main.selected_stream_source = Some(source);
            if start_after_pick {
                start_screen_share_from_selected_source(
                    ctx,
                    state,
                    api,
                    engine_tx.as_ref(),
                    state.user_id,
                );
            } else {
                ctx.request_repaint();
            }
        }
        if close_picker {
            state.main.show_screen_source_picker = false;
            state.main.start_stream_after_source_pick = false;
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
                let viewport = ctx
                    .input(|i| i.viewport().inner_rect)
                    .unwrap_or_else(|| ctx.screen_rect());
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, viewport.size());
                ui.allocate_rect(screen, egui::Sense::hover()); // Ensure Area covers full window
                ui.painter().rect_filled(
                    screen,
                    egui::Rounding::ZERO,
                    egui::Color32::from_black_alpha(240),
                );
                let stream_key = video_frame_key(uid, true);
                // Phase 3.5: prefer GPU zero-copy texture, fall back to CPU-uploaded.
                let shown = if let Some(&(_, gl_tex_id, w, h)) =
                    state.main.voice_video_gpu_textures.get(&stream_key)
                {
                    let max_w = screen.width();
                    let max_h = screen.height();
                    // Scale to fill window; allow upscaling when window is larger than video.
                    let scale = (max_w / w as f32).min(max_h / h as f32);
                    let size = egui::vec2(w as f32 * scale, h as f32 * scale);
                    let pos = screen.center() - size / 2.0;
                    paint_wgl_video_texture(ui, egui::Rect::from_min_size(pos, size), gl_tex_id);
                    true
                } else if let Some(tex) = state.main.voice_video_textures.get(&stream_key) {
                    let tex_size = tex.size_vec2();
                    let max_w = screen.width();
                    let max_h = screen.height();
                    // Scale to fill window; allow upscaling when window is larger than video.
                    let scale = (max_w / tex_size.x).min(max_h / tex_size.y);
                    let size = tex_size * scale;
                    let pos = screen.center() - size / 2.0;
                    ui.put(
                        egui::Rect::from_min_size(pos, size),
                        egui::Image::new(tex).fit_to_exact_size(size),
                    );
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
                    let fps = state
                        .main
                        .voice_render_fps
                        .get_mut(&stream_key)
                        .map(|t| t.update_and_get())
                        .unwrap_or(0.0);
                    if fps > 0.0 {
                        let fps_text = format!("{:.0} fps", fps);
                        let pos = screen.left_bottom() + egui::vec2(16.0, -28.0);
                        let size = egui::vec2(48.0, 22.0);
                        let bg_rect = egui::Rect::from_min_size(pos, size);
                        ui.painter().rect_filled(
                            bg_rect,
                            egui::Rounding::same(4.0),
                            egui::Color32::from_black_alpha(200),
                        );
                        ui.painter().text(
                            pos + egui::vec2(8.0, 4.0),
                            egui::Align2::LEFT_TOP,
                            fps_text,
                            egui::FontId::proportional(14.0),
                            egui::Color32::WHITE,
                        );
                    }
                }
                let controls_rect = egui::Rect::from_min_size(
                    screen.left_top() + egui::vec2(16.0, 16.0),
                    egui::vec2(220.0, 36.0),
                );
                ui.allocate_ui_at_rect(controls_rect, |ui| {
                    ui.horizontal(|ui| {
                        show_stream_audio_button(ui, state, engine_tx, uid);
                        if ui.button("⛶ Закрыть полноэкранный режим").clicked()
                        {
                            state.main.fullscreen_stream_user = None;
                        }
                    });
                });
            });
    }

    should_logout
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn effective_user_volume(voice: &VoiceState, user_id: i64) -> f32 {
    if voice.locally_muted.contains(&user_id) {
        0.0
    } else {
        voice.local_volumes.get(&user_id).copied().unwrap_or(1.0)
    }
}

fn sync_user_volume(
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    voice: &VoiceState,
    user_id: i64,
) {
    if let Some(tx) = engine_tx.as_ref() {
        tx.send(VoiceCmd::SetUserVolume(
            user_id,
            effective_user_volume(voice, user_id),
        ))
        .ok();
    }
}

fn effective_stream_volume(voice: &VoiceState, user_id: i64) -> f32 {
    if voice.stream_muted.contains(&user_id) {
        0.0
    } else {
        voice.stream_volumes.get(&user_id).copied().unwrap_or(1.0)
    }
}

fn sync_stream_volume(
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    voice: &VoiceState,
    user_id: i64,
) {
    if let Some(tx) = engine_tx.as_ref() {
        tx.send(VoiceCmd::SetStreamVolume(
            user_id,
            effective_stream_volume(voice, user_id),
        ))
        .ok();
    }
}

fn sync_receiver_denoise(
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    voice: &VoiceState,
    user_id: i64,
) {
    if let Some(tx) = engine_tx.as_ref() {
        tx.send(VoiceCmd::SetRemoteVoiceDenoise {
            user_id,
            enabled: voice.receiver_denoise_users.contains(&user_id),
        })
        .ok();
    }
}

fn set_receiver_denoise_enabled(
    state: &mut State,
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: i64,
    enabled: bool,
) {
    if enabled {
        state.main.voice.receiver_denoise_users.insert(user_id);
        state
            .settings
            .receiver_denoise_by_user
            .insert(user_id.to_string());
    } else {
        state.main.voice.receiver_denoise_users.remove(&user_id);
        state
            .settings
            .receiver_denoise_by_user
            .remove(&user_id.to_string());
    }
    state.settings.save();
    sync_receiver_denoise(engine_tx, &state.main.voice, user_id);
}

fn set_stream_subscription(
    state: &mut State,
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: i64,
    subscribed: bool,
) {
    if subscribed {
        state.main.voice.stream_subscriptions.insert(user_id);
    } else {
        state.main.voice.stream_subscriptions.remove(&user_id);
        state.main.voice.stream_muted.remove(&user_id);
    }
    if let Some(tx) = engine_tx.as_ref() {
        tx.send(VoiceCmd::SetStreamSubscription {
            user_id,
            subscribed,
        })
        .ok();
    }
    if subscribed {
        sync_stream_volume(engine_tx, &state.main.voice, user_id);
    }
}

fn show_stream_audio_button(
    ui: &mut egui::Ui,
    state: &mut State,
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: i64,
) {
    let muted = state.main.voice.stream_muted.contains(&user_id);
    let response = ui
        .button(if muted { "🔇" } else { "🔊" })
        .on_hover_text(if muted {
            "Включить звук трансляции"
        } else {
            "Выключить звук трансляции"
        });
    if response.clicked() {
        if muted {
            state.main.voice.stream_muted.remove(&user_id);
        } else {
            state.main.voice.stream_muted.insert(user_id);
        }
        sync_stream_volume(engine_tx, &state.main.voice, user_id);
    }
    response.context_menu(|ui| {
        ui.label("Громкость трансляции 0-400%, по умолчанию 100%");
        let mut volume = state
            .main
            .voice
            .stream_volumes
            .get(&user_id)
            .copied()
            .unwrap_or(1.0);
        if ui
            .add(
                egui::Slider::new(&mut volume, 0.0..=4.0)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .text(""),
            )
            .changed()
        {
            let volume = volume.clamp(0.0, 4.0);
            state.main.voice.stream_volumes.insert(user_id, volume);
            state
                .settings
                .stream_volume_by_user
                .insert(user_id.to_string(), volume);
            state.settings.save();
            state.main.voice.stream_muted.remove(&user_id);
            sync_stream_volume(engine_tx, &state.main.voice, user_id);
        }
    });
}

fn mime_from_path(path: &PathBuf) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("avi") => "video/avi",
        Some("zip") => "application/zip",
        Some("rar") => "application/x-rar-compressed",
        Some("7z") => "application/x-7z-compressed",
        Some("tar") => "application/x-tar",
        Some("gz") => "application/gzip",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
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
        if let Some(save_path) = rfd::FileDialog::new().set_file_name(filename).save_file() {
            let _ = std::fs::write(save_path, bytes);
        }
    }
}
