use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
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

const VOICE_LATENCY_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const VOICE_LATENCY_HISTORY_WINDOW: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub(crate) struct VoiceLatencySample {
    recorded_at: Instant,
    latency_ms: f32,
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
    new_event_queue, ws_task, ApiClient, ApiError, AttachmentMeta, Channel, InvitePreview,
    LoginRequest, Member, Message, RegisterRequest, Server, VoiceParticipant, WsClientMsg,
    WsEventQueue,
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
    #[serde(default = "default_input_volume")]
    pub(crate) input_volume: f32,
    #[serde(default = "default_input_sensitivity")]
    pub(crate) input_sensitivity: f32,
    #[serde(default = "default_output_volume")]
    pub(crate) output_volume: f32,
    #[serde(default)]
    pub(crate) screen_preset: crate::voice::ScreenPreset,
    /// Путь декодирования входящего видео: "cpu" (OpenH264) или "mft" (Media Foundation).
    #[serde(default)]
    pub(crate) decode_path: String,
    /// Retired legacy gamma override kept only for config migration. It is forced to 0 on load.
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

fn default_input_sensitivity() -> f32 {
    0.55
}

fn default_input_volume() -> f32 {
    1.0
}

fn normalize_input_volume(value: f32) -> f32 {
    value.clamp(0.0, 2.0)
}

fn normalize_input_sensitivity(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

fn default_output_volume() -> f32 {
    1.0
}

fn normalize_output_volume(value: f32) -> f32 {
    value.clamp(0.0, 4.0)
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
            input_volume: default_input_volume(),
            input_sensitivity: default_input_sensitivity(),
            output_volume: default_output_volume(),
            screen_preset: crate::voice::ScreenPreset::default(),
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
        let normalized_input_volume = normalize_input_volume(s.input_volume);
        if (s.input_volume - normalized_input_volume).abs() > f32::EPSILON {
            s.input_volume = normalized_input_volume;
            should_save = true;
        }
        let normalized_input_sensitivity = normalize_input_sensitivity(s.input_sensitivity);
        if (s.input_sensitivity - normalized_input_sensitivity).abs() > f32::EPSILON {
            s.input_sensitivity = normalized_input_sensitivity;
            should_save = true;
        }
        let normalized_output_volume = normalize_output_volume(s.output_volume);
        if (s.output_volume - normalized_output_volume).abs() > f32::EPSILON {
            s.output_volume = normalized_output_volume;
            should_save = true;
        }
        if s.migrate_saved_accounts() {
            should_save = true;
        }
        if s.video_decoder_gamma.abs() > f32::EPSILON {
            eprintln!(
                "[video] clearing retired decoder gamma override {:.2} -> 0.00",
                s.video_decoder_gamma
            );
            s.video_decoder_gamma = 0.0;
            should_save = true;
        }
        if !s.video_decoder_gamma_migrated_v2 {
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

fn clear_chat_search_state(main: &mut MainState) {
    main.chat_search_results.clear();
    main.chat_search_popup_open = false;
    main.chat_search_scroll_offset = 0.0;
    main.chat_search_load = LoadState::Idle;
    main.chat_search_started_server = None;
    main.chat_search_started_query.clear();
    main.chat_search_request_seq = main.chat_search_request_seq.wrapping_add(1);
}

fn contains_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    haystack.to_lowercase().contains(needle_lower)
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
            if st.main.selected_channel.is_none() {
                st.main.selected_channel = st
                    .main
                    .channels
                    .iter()
                    .find(|channel| channel.r#type == "text")
                    .or_else(|| st.main.channels.first())
                    .map(|channel| channel.id);
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

pub(crate) fn process_message_search(ctx: egui::Context, state: Arc<Mutex<State>>, api: ApiClient) {
    let (token, server_id, channels, query, channels_ready) = {
        let st = state.lock();
        if st.screen != Screen::Main {
            return;
        }
        let token = match &st.access_token {
            Some(token) => token.clone(),
            None => return,
        };
        (
            token,
            st.main.selected_server,
            st.main.channels.clone(),
            st.main.chat_search_query.trim().to_string(),
            st.main.channels_load == LoadState::Loaded,
        )
    };

    let Some(server_id) = server_id else {
        let mut st = state.lock();
        clear_chat_search_state(&mut st.main);
        return;
    };

    if query.is_empty() {
        let mut st = state.lock();
        clear_chat_search_state(&mut st.main);
        return;
    }

    if !channels_ready {
        let mut st = state.lock();
        st.main.chat_search_results.clear();
        st.main.chat_search_scroll_offset = 0.0;
        st.main.chat_search_load = LoadState::Loading;
        st.main.chat_search_started_server = None;
        st.main.chat_search_started_query.clear();
        return;
    }

    let text_channels: Vec<(i64, String)> = channels
        .into_iter()
        .filter(|channel| channel.r#type == "text")
        .map(|channel| (channel.id, channel.name))
        .collect();

    let request_seq = {
        let mut st = state.lock();
        let already_running = st.main.chat_search_started_server == Some(server_id)
            && st.main.chat_search_started_query == query
            && matches!(
                st.main.chat_search_load,
                LoadState::Loading | LoadState::Loaded | LoadState::Error(_)
            );
        if already_running {
            return;
        }
        st.main.chat_search_request_seq = st.main.chat_search_request_seq.wrapping_add(1);
        st.main.chat_search_started_server = Some(server_id);
        st.main.chat_search_started_query = query.clone();
        st.main.chat_search_results.clear();
        st.main.chat_search_scroll_offset = 0.0;
        st.main.chat_search_load = LoadState::Loading;
        st.main.chat_search_request_seq
    };

    let state_c = Arc::clone(&state);
    let ctx_c = ctx.clone();
    tokio::spawn(async move {
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for (channel_id, channel_name) in text_channels {
            {
                let st = state_c.lock();
                if st.main.chat_search_request_seq != request_seq
                    || st.main.selected_server != Some(server_id)
                    || st.main.chat_search_query.trim() != query
                {
                    return;
                }
            }

            let fetch = tokio::time::timeout(
                Duration::from_secs(LOAD_TIMEOUT_SECS),
                api.list_messages(&token, channel_id),
            )
            .await;

            let messages = match fetch {
                Ok(Ok(messages)) => messages,
                Ok(Err(err)) => {
                    let mut st = state_c.lock();
                    if st.main.chat_search_request_seq == request_seq {
                        st.main.chat_search_load =
                            LoadState::Error(format!("Ошибка поиска: {err}"));
                    }
                    ctx_c.request_repaint();
                    return;
                }
                Err(_) => {
                    let mut st = state_c.lock();
                    if st.main.chat_search_request_seq == request_seq {
                        st.main.chat_search_load =
                            LoadState::Error("Таймаут поиска сообщений".to_string());
                    }
                    ctx_c.request_repaint();
                    return;
                }
            };

            results.extend(messages.into_iter().filter_map(|message| {
                if contains_case_insensitive(&message.content, &query_lower) {
                    Some(chat_panel::ChatSearchResult {
                        channel_id,
                        channel_name: channel_name.clone(),
                        message,
                    })
                } else {
                    None
                }
            }));
        }

        results.sort_by(|left, right| left.message.created_at.cmp(&right.message.created_at));

        let mut st = state_c.lock();
        if st.main.chat_search_request_seq != request_seq
            || st.main.selected_server != Some(server_id)
            || st.main.chat_search_query.trim() != query
        {
            return;
        }
        st.main.chat_search_results = results;
        st.main.chat_search_load = LoadState::Loaded;
        ctx_c.request_repaint();
    });
}

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
    pub(crate) deafened_via_toggle: bool,
    pub(crate) local_volumes: HashMap<i64, f32>,
    pub(crate) locally_muted: HashSet<i64>,
    pub(crate) stream_volumes: HashMap<i64, f32>,
    pub(crate) stream_muted: HashSet<i64>,
    pub(crate) stream_subscriptions: HashSet<i64>,
    pub(crate) receiver_denoise_users: HashSet<i64>,
    pub(crate) speaking: Arc<Mutex<HashMap<i64, bool>>>,
    pub(crate) input_volume: f32,
    pub(crate) input_sensitivity: f32,
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
            deafened_via_toggle: false,
            local_volumes: HashMap::new(),
            locally_muted: HashSet::new(),
            stream_volumes: HashMap::new(),
            stream_muted: HashSet::new(),
            stream_subscriptions: HashSet::new(),
            receiver_denoise_users: HashSet::new(),
            speaking: Arc::new(Mutex::new(HashMap::new())),
            input_volume: 1.0,
            input_sensitivity: default_input_sensitivity(),
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

pub(crate) struct ScreenSourcePreviewFrame {
    target: StreamSourceTarget,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

const SCREEN_SOURCE_TILE_ASPECT: f32 = 16.0 / 9.0;
const SCREEN_SOURCE_TILE_MAX_COLUMNS: usize = 3;
const SCREEN_SOURCE_TILE_GAP: f32 = 12.0;
const SCREEN_SOURCE_TILE_LABEL_HEIGHT: f32 = 42.0;
const SCREEN_SOURCE_PREVIEW_MAX_DIM: u32 = 640;
const SCREEN_SOURCE_PREVIEW_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const SCREEN_SOURCE_PICKER_HORIZONTAL_MARGIN: f32 = 100.0;
const SCREEN_SOURCE_PICKER_VERTICAL_MARGIN: f32 = 50.0;
const SCREEN_SOURCE_LABEL_SCREEN: &str = "\u{42d}\u{43a}\u{440}\u{430}\u{43d}";
const SCREEN_SOURCE_LABEL_PRIMARY: &str = "\u{43e}\u{441}\u{43d}\u{43e}\u{432}\u{43d}\u{43e}\u{439}";
const SCREEN_SOURCE_LABEL_UNNAMED_WINDOW: &str =
    "\u{41e}\u{43a}\u{43d}\u{43e} \u{431}\u{435}\u{437} \u{43d}\u{430}\u{437}\u{432}\u{430}\u{43d}\u{438}\u{44f}";
const SCREEN_SOURCE_LABEL_LOADING_PREVIEW: &str =
    "\u{417}\u{430}\u{433}\u{440}\u{443}\u{436}\u{430}\u{435}\u{43c}\n\u{43f}\u{440}\u{435}\u{432}\u{44c}\u{44e}...";
const SCREEN_SOURCE_LABEL_PREVIEW_UNAVAILABLE: &str =
    "\u{41f}\u{440}\u{435}\u{432}\u{44c}\u{44e}\n\u{43d}\u{435}\u{434}\u{43e}\u{441}\u{442}\u{443}\u{43f}\u{43d}\u{43e}";
const SCREEN_SOURCE_PICKER_TITLE: &str =
    "\u{412}\u{44b}\u{431}\u{43e}\u{440} \u{438}\u{441}\u{442}\u{43e}\u{447}\u{43d}\u{438}\u{43a}\u{430} \u{442}\u{440}\u{430}\u{43d}\u{441}\u{43b}\u{44f}\u{446}\u{438}\u{438}";
const SCREEN_SOURCE_TAB_APPLICATIONS: &str =
    "\u{41f}\u{440}\u{438}\u{43b}\u{43e}\u{436}\u{435}\u{43d}\u{438}\u{44f}";
const SCREEN_SOURCE_TAB_ENTIRE_SCREEN: &str =
    "\u{412}\u{435}\u{441}\u{44c} \u{44d}\u{43a}\u{440}\u{430}\u{43d}";
const SCREEN_SOURCE_LABEL_NO_WINDOWS: &str =
    "\u{41f}\u{43e}\u{434}\u{445}\u{43e}\u{434}\u{44f}\u{449}\u{438}\u{435} \u{43e}\u{43a}\u{43d}\u{430} \u{43d}\u{435} \u{43d}\u{430}\u{439}\u{434}\u{435}\u{43d}\u{44b}.";
const SCREEN_SOURCE_LABEL_NO_SCREENS: &str =
    "\u{42d}\u{43a}\u{440}\u{430}\u{43d}\u{44b} \u{43d}\u{435} \u{43e}\u{431}\u{43d}\u{430}\u{440}\u{443}\u{436}\u{435}\u{43d}\u{44b}.";
const SCREEN_SOURCE_PICKER_HELP_TEXT: &str =
    "\u{412}\u{44b}\u{431}\u{43e}\u{440} \u{438}\u{441}\u{442}\u{43e}\u{447}\u{43d}\u{438}\u{43a}\u{430} \u{43d}\u{435} \u{437}\u{430}\u{43f}\u{443}\u{441}\u{43a}\u{430}\u{435}\u{442} \u{441}\u{442}\u{440}\u{438}\u{43c}. \u{414}\u{43b}\u{44f} \u{437}\u{430}\u{43f}\u{443}\u{441}\u{43a}\u{430} \u{438}\u{441}\u{43f}\u{43e}\u{43b}\u{44c}\u{437}\u{443}\u{439}\u{442}\u{435} \u{43a}\u{43d}\u{43e}\u{43f}\u{43a}\u{443} \"\u{41d}\u{430}\u{447}\u{430}\u{442}\u{44c} \u{442}\u{440}\u{430}\u{43d}\u{441}\u{43b}\u{44f}\u{446}\u{438}\u{44e}\".";
const SCREEN_SOURCE_ACTION_BUTTON_LABEL: &str =
    "\u{414}\u{435}\u{43c}\u{43e}\u{43d}\u{441}\u{442}\u{440}\u{438}\u{440}\u{43e}\u{432}\u{430}\u{442}\u{44c} \u{44d}\u{43a}\u{440}\u{430}\u{43d}";
const SCREEN_SOURCE_ACTION_BUTTON_LABEL_SHORT: &str =
    "\u{41f}\u{43e}\u{43a}\u{430}\u{437}\u{430}\u{442}\u{44c} \u{44d}\u{43a}\u{440}\u{430}\u{43d}";
const SCREEN_SOURCE_ACTION_BUTTON_LABEL_COMPACT: &str =
    "\u{41f}\u{43e}\u{43a}\u{430}\u{437}\u{430}\u{442}\u{44c}";
const SCREEN_SOURCE_ACTION_BUTTON_LABEL_MINIMAL: &str =
    "\u{421}\u{442}\u{430}\u{440}\u{442}";
const SCREEN_SOURCE_CANCEL_LABEL: &str =
    "\u{41e}\u{442}\u{43c}\u{435}\u{43d}\u{430}";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    Account,
    VoiceVideo,
    Application,
}

impl Default for SettingsSection {
    fn default() -> Self {
        Self::Account
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerSettingsSection {
    General,
    BanList,
}

impl Default for ServerSettingsSection {
    fn default() -> Self {
        Self::General
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
    pub(crate) chat_search_query: String,
    pub(crate) chat_search_popup_open: bool,
    pub(crate) chat_search_results: Vec<chat_panel::ChatSearchResult>,
    pub(crate) chat_search_scroll_offset: f32,
    pub(crate) chat_search_load: LoadState,
    pub(crate) chat_search_started_server: Option<i64>,
    pub(crate) chat_search_started_query: String,
    pub(crate) chat_search_request_seq: u64,
    pub(crate) highlighted_message_channel_id: Option<i64>,
    pub(crate) highlighted_message_id: Option<i64>,
    pub(crate) highlighted_message_until: Option<Instant>,
    pub(crate) highlighted_message_scroll_pending: bool,
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
    pub(crate) show_server_settings_dialog: bool,
    pub(crate) show_console_dialog: bool,
    pub(crate) settings_section: SettingsSection,
    pub(crate) server_settings_section: ServerSettingsSection,
    pub(crate) new_channel_name: String,
    pub(crate) new_channel_is_voice: bool,
    pub(crate) current_channel_key: Option<ChannelKey>,
    pub(crate) server_to_delete: Option<i64>,
    pub(crate) invite_user_id_input: String,
    pub(crate) invite_msg: Option<String>,
    pub(crate) invite_link: Option<String>,
    pub(crate) ws_connected_server: Option<i64>,
    pub(crate) ws_viewing_channel: Option<i64>,
    pub(crate) typing_users: Vec<(i64, String, Instant)>,
    pub(crate) last_typing_sent: Option<Instant>,
    pub(crate) online_users: HashSet<i64>,
    pub(crate) my_display_name: String,
    pub(crate) settings_nickname_input: String,
    pub(crate) settings_avatar_path: Option<PathBuf>,
    pub(crate) settings_msg: Option<String>,
    pub(crate) server_settings_name_input: String,
    pub(crate) server_settings_msg: Option<String>,
    pub(crate) server_bans: Vec<Member>,
    pub(crate) server_bans_load: LoadState,
    pub(crate) pending_invite_token: Option<String>,
    pub(crate) pending_invite_preview: Option<InvitePreview>,
    pub(crate) pending_invite_status: LoadState,
    pub(crate) pending_invite_msg: Option<String>,
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
    pub(crate) voice_grid_focus_user: Option<i64>,
    /// Debounce: stream keys seen as non-streaming in previous frame. Remove texture only after 2 consecutive frames.
    pub(crate) stream_ended_prev_frame: HashSet<i64>,
    pub(crate) show_screen_source_picker: bool,
    pub(crate) screen_sources: Vec<ScreenSourceEntry>,
    pub(crate) window_sources: Vec<ScreenSourceEntry>,
    pub(crate) selected_stream_source: Option<ScreenSourceEntry>,
    pub(crate) start_stream_after_source_pick: bool,
    pub(crate) screen_source_tab: ScreenSourceTab,
    pub(crate) screen_source_preview_textures:
        HashMap<StreamSourceTarget, egui::TextureHandle>,
    pub(crate) screen_source_preview_rx: Option<mpsc::Receiver<Vec<ScreenSourcePreviewFrame>>>,
    pub(crate) screen_source_preview_inflight: bool,
    pub(crate) screen_source_preview_requested_tab: Option<ScreenSourceTab>,
    pub(crate) screen_source_preview_last_refresh: Option<Instant>,
    pub(crate) screen_preset: crate::voice::ScreenPreset,
    pub(crate) voice_stats: Option<Arc<Mutex<VoiceSessionStats>>>,
    pub(crate) voice_latency_history: VecDeque<VoiceLatencySample>,
    pub(crate) last_voice_latency_sample_at: Option<Instant>,
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
            chat_search_query: self.chat_search_query.clone(),
            chat_search_popup_open: self.chat_search_popup_open,
            chat_search_results: self.chat_search_results.clone(),
            chat_search_scroll_offset: self.chat_search_scroll_offset,
            chat_search_load: self.chat_search_load.clone(),
            chat_search_started_server: self.chat_search_started_server,
            chat_search_started_query: self.chat_search_started_query.clone(),
            chat_search_request_seq: self.chat_search_request_seq,
            highlighted_message_channel_id: self.highlighted_message_channel_id,
            highlighted_message_id: self.highlighted_message_id,
            highlighted_message_until: self.highlighted_message_until,
            highlighted_message_scroll_pending: self.highlighted_message_scroll_pending,
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
            show_server_settings_dialog: self.show_server_settings_dialog,
            show_console_dialog: self.show_console_dialog,
            settings_section: self.settings_section,
            server_settings_section: self.server_settings_section,
            new_channel_name: self.new_channel_name.clone(),
            new_channel_is_voice: self.new_channel_is_voice,
            current_channel_key: self.current_channel_key.clone(),
            server_to_delete: self.server_to_delete,
            invite_user_id_input: self.invite_user_id_input.clone(),
            invite_msg: self.invite_msg.clone(),
            invite_link: self.invite_link.clone(),
            ws_connected_server: self.ws_connected_server,
            ws_viewing_channel: self.ws_viewing_channel,
            typing_users: self.typing_users.clone(),
            last_typing_sent: self.last_typing_sent,
            online_users: self.online_users.clone(),
            my_display_name: self.my_display_name.clone(),
            settings_nickname_input: self.settings_nickname_input.clone(),
            settings_avatar_path: self.settings_avatar_path.clone(),
            settings_msg: self.settings_msg.clone(),
            server_settings_name_input: self.server_settings_name_input.clone(),
            server_settings_msg: self.server_settings_msg.clone(),
            server_bans: self.server_bans.clone(),
            server_bans_load: self.server_bans_load.clone(),
            pending_invite_token: self.pending_invite_token.clone(),
            pending_invite_preview: self.pending_invite_preview.clone(),
            pending_invite_status: self.pending_invite_status.clone(),
            pending_invite_msg: self.pending_invite_msg.clone(),
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
            voice_grid_focus_user: None,
            stream_ended_prev_frame: HashSet::new(),
            show_screen_source_picker: false,
            screen_sources: Vec::new(),
            window_sources: Vec::new(),
            selected_stream_source: self.selected_stream_source.clone(),
            start_stream_after_source_pick: false,
            screen_source_tab: self.screen_source_tab,
            screen_source_preview_textures: HashMap::new(),
            screen_source_preview_rx: None,
            screen_source_preview_inflight: false,
            screen_source_preview_requested_tab: None,
            screen_source_preview_last_refresh: None,
            screen_preset: self.screen_preset,
            voice_stats: self.voice_stats.clone(), // Arc clone
            voice_latency_history: self.voice_latency_history.clone(),
            last_voice_latency_sample_at: self.last_voice_latency_sample_at,
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
            chat_search_query: String::new(),
            chat_search_popup_open: false,
            chat_search_results: Vec::new(),
            chat_search_scroll_offset: 0.0,
            chat_search_load: LoadState::Idle,
            chat_search_started_server: None,
            chat_search_started_query: String::new(),
            chat_search_request_seq: 0,
            highlighted_message_channel_id: None,
            highlighted_message_id: None,
            highlighted_message_until: None,
            highlighted_message_scroll_pending: false,
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
            show_server_settings_dialog: false,
            show_console_dialog: false,
            settings_section: SettingsSection::default(),
            server_settings_section: ServerSettingsSection::default(),
            new_channel_name: String::new(),
            new_channel_is_voice: false,
            current_channel_key: None,
            server_to_delete: None,
            invite_user_id_input: String::new(),
            invite_msg: None,
            invite_link: None,
            ws_connected_server: None,
            ws_viewing_channel: None,
            typing_users: Vec::new(),
            last_typing_sent: None,
            online_users: HashSet::new(),
            my_display_name: String::new(),
            settings_nickname_input: String::new(),
            settings_avatar_path: None,
            settings_msg: None,
            server_settings_name_input: String::new(),
            server_settings_msg: None,
            server_bans: Vec::new(),
            server_bans_load: LoadState::Idle,
            pending_invite_token: None,
            pending_invite_preview: None,
            pending_invite_status: LoadState::Idle,
            pending_invite_msg: None,
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
            voice_grid_focus_user: None,
            stream_ended_prev_frame: HashSet::new(),
            show_screen_source_picker: false,
            screen_sources: Vec::new(),
            window_sources: Vec::new(),
            selected_stream_source: None,
            start_stream_after_source_pick: false,
            screen_source_tab: ScreenSourceTab::default(),
            screen_source_preview_textures: HashMap::new(),
            screen_source_preview_rx: None,
            screen_source_preview_inflight: false,
            screen_source_preview_requested_tab: None,
            screen_source_preview_last_refresh: None,
            screen_preset: crate::voice::ScreenPreset::default(),
            voice_stats: None,
            voice_latency_history: VecDeque::new(),
            last_voice_latency_sample_at: None,
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

pub(crate) fn apply_persisted_media_preferences(state: &mut State) {
    state.main.voice.input_volume = normalize_input_volume(state.settings.input_volume);
    state.main.voice.input_sensitivity =
        normalize_input_sensitivity(state.settings.input_sensitivity);
    state.main.voice.output_volume = normalize_output_volume(state.settings.output_volume);
    state.main.screen_preset = state.settings.screen_preset;
}

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

fn show_modal_backdrop(ctx: &egui::Context, id: impl std::hash::Hash, rect: egui::Rect) {
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Middle)
        .movable(false)
        .fixed_pos(rect.min)
        .show(ctx, |ui| {
            let (rect, response) =
                ui.allocate_exact_size(rect.size(), egui::Sense::click_and_drag());
            response.surrender_focus();
            ui.painter().rect_filled(
                rect,
                egui::Rounding::ZERO,
                egui::Color32::from_black_alpha(170),
            );
        });
}

fn window_inner_size_for_outer(ctx: &egui::Context, outer_size: egui::Vec2, frame: &egui::Frame) -> egui::Vec2 {
    let window_margin = frame.inner_margin;
    let window_border_padding = frame.stroke.width / 2.0;
    let mut window_inner_margin = window_margin;
    window_inner_margin += window_border_padding;
    let title_bar_height = egui::TextStyle::Heading
        .resolve(ctx.style().as_ref())
        .size
        .max(ctx.style().spacing.interact_size.y)
        + window_margin.top
        + window_margin.bottom;
    let window_chrome =
        frame.outer_margin.sum() + window_inner_margin.sum() + egui::vec2(0.0, title_bar_height);
    egui::vec2(
        (outer_size.x - window_chrome.x).max(0.0),
        (outer_size.y - window_chrome.y).max(0.0),
    )
}

fn inset_modal_rect(
    rect: egui::Rect,
    horizontal_margin: f32,
    vertical_margin: f32,
    bottom_reserved: f32,
) -> egui::Rect {
    let bottom_reserved = bottom_reserved.min((rect.height() - 1.0).max(0.0));
    let horizontal_margin = horizontal_margin.min(((rect.width() - 1.0) * 0.5).max(0.0));
    let available_height = (rect.height() - bottom_reserved).max(1.0);
    let vertical_margin = vertical_margin.min(((available_height - 1.0) * 0.5).max(0.0));
    egui::Rect::from_min_max(
        egui::pos2(rect.left() + horizontal_margin, rect.top() + vertical_margin),
        egui::pos2(
            rect.right() - horizontal_margin,
            rect.bottom() - bottom_reserved - vertical_margin,
        ),
    )
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
                reset_voice_connection_metrics(state);
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
                tx.send(VoiceCmd::SetInputSensitivity(
                    state.main.voice.input_sensitivity,
                ))
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

fn reset_voice_connection_metrics(state: &mut State) {
    state.main.voice_latency_history.clear();
    state.main.last_voice_latency_sample_at = None;
    if let Some(stats) = state.main.voice_stats.as_ref() {
        *stats.lock() = VoiceSessionStats::default();
    }
}

fn sample_voice_latency_history(state: &mut State) {
    let now = Instant::now();
    if state.main.voice.channel_id.is_none() {
        state.main.voice_latency_history.clear();
        state.main.last_voice_latency_sample_at = None;
        return;
    }

    while state
        .main
        .voice_latency_history
        .front()
        .map(|sample| now.duration_since(sample.recorded_at) > VOICE_LATENCY_HISTORY_WINDOW)
        .unwrap_or(false)
    {
        state.main.voice_latency_history.pop_front();
    }

    let latency_ms = state
        .main
        .voice_stats
        .as_ref()
        .and_then(|stats| stats.lock().latency_rtt_ms)
        .filter(|latency_ms| latency_ms.is_finite() && *latency_ms >= 0.0);

    let should_sample = state
        .main
        .last_voice_latency_sample_at
        .map(|last| now.duration_since(last) >= VOICE_LATENCY_SAMPLE_INTERVAL)
        .unwrap_or(true);
    if should_sample {
        if let Some(latency_ms) = latency_ms {
            state
                .main
                .voice_latency_history
                .push_back(VoiceLatencySample {
                    recorded_at: now,
                    latency_ms,
                });
            state.main.last_voice_latency_sample_at = Some(now);
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
    reset_voice_connection_metrics(state);
    state.main.voice = VoiceState::default();
    apply_persisted_media_preferences(state);
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
    state.main.voice_grid_focus_user = None;
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
            current_voice_deafened(&state.main.voice),
            state.main.voice.camera_on,
            state.main.voice.screen_on,
        ));
    }
}

fn current_voice_deafened(voice: &VoiceState) -> bool {
    voice.mic_muted && voice.output_muted
}

fn selected_server_owner_id(state: &State) -> Option<i64> {
    state
        .main
        .servers
        .iter()
        .find(|server| Some(server.id) == state.main.selected_server)
        .map(|server| server.owner_id)
}

fn can_manage_server_member(state: &State, target_user_id: i64) -> bool {
    let Some(current_user_id) = state.user_id else {
        return false;
    };
    let Some(owner_id) = selected_server_owner_id(state) else {
        return false;
    };
    current_user_id == owner_id && target_user_id != current_user_id && target_user_id != owner_id
}

fn open_server_settings(state: &mut State) {
    state.main.show_server_settings_dialog = true;
    state.main.server_settings_section = ServerSettingsSection::General;
    state.main.server_settings_msg = None;
    state.main.server_bans_load = LoadState::Idle;
    state.main.server_bans.clear();
    state.main.server_settings_name_input = state
        .main
        .servers
        .iter()
        .find(|server| Some(server.id) == state.main.selected_server)
        .map(|server| server.name.clone())
        .unwrap_or_default();
}

fn kick_server_member(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    target_user_id: i64,
) {
    if !can_manage_server_member(state, target_user_id) {
        return;
    }
    if let (Some(server_id), Some(ref token)) = (state.main.selected_server, &state.access_token) {
        let _ = block_on(api.kick_member(token, server_id, target_user_id));
        ctx.request_repaint();
    }
}

fn ban_server_member(ctx: &egui::Context, state: &mut State, api: &ApiClient, target_user_id: i64) {
    if !can_manage_server_member(state, target_user_id) {
        return;
    }
    if let (Some(server_id), Some(ref token)) = (state.main.selected_server, &state.access_token) {
        let _ = block_on(api.ban_member(token, server_id, target_user_id));
        ctx.request_repaint();
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

fn update_local_deafened_flag(state: &mut State, user_id: Option<i64>, deafened: bool) {
    if let Some(uid) = user_id {
        for participant in &mut state.main.voice.participants {
            if participant.user_id == uid {
                participant.deafened = deafened;
                break;
            }
        }
        if let Some(ch_id) = state.main.voice.channel_id {
            if let Some(list) = state.main.channel_voice.get_mut(&ch_id) {
                for participant in list.iter_mut() {
                    if participant.user_id == uid {
                        participant.deafened = deafened;
                        break;
                    }
                }
            }
        }
    }
}

#[allow(dead_code)]
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

fn refresh_screen_share_sources(state: &mut State) {
    let monitors = crate::voice_livekit::enumerate_unique_screens();
    state.main.screen_sources = monitors
        .iter()
        .enumerate()
        .map(|(i, monitor)| ScreenSourceEntry {
            label: if monitor.is_primary() {
                format!("{SCREEN_SOURCE_LABEL_SCREEN} {} ({SCREEN_SOURCE_LABEL_PRIMARY})", i + 1)
            } else {
                format!("{SCREEN_SOURCE_LABEL_SCREEN} {}", i + 1)
            },
            target: StreamSourceTarget::Monitor { index: i },
        })
        .collect();
    state.main.window_sources = crate::voice_livekit::enumerate_stream_windows()
        .into_iter()
        .map(|window| ScreenSourceEntry {
            label: screen_share_window_label(&window.title),
            target: StreamSourceTarget::Window {
                window_id: window.window_id,
                process_id: window.process_id,
            },
        })
        .collect();
    retain_screen_source_preview_textures(state);
}

fn screen_share_window_label(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        SCREEN_SOURCE_LABEL_UNNAMED_WINDOW.to_string()
    } else {
        title.to_string()
    }
}

fn screen_source_texture_name(target: &StreamSourceTarget) -> String {
    match target {
        StreamSourceTarget::Monitor { index } => {
            format!("screen_source_preview_monitor_{index}")
        }
        StreamSourceTarget::Window {
            window_id,
            process_id,
        } => format!("screen_source_preview_window_{window_id}_{process_id}"),
    }
}

fn retain_screen_source_preview_textures(state: &mut State) {
    let valid_targets: HashSet<_> = state
        .main
        .screen_sources
        .iter()
        .chain(state.main.window_sources.iter())
        .map(|source| source.target.clone())
        .collect();
    state
        .main
        .screen_source_preview_textures
        .retain(|target, _| valid_targets.contains(target));
}

fn invalidate_screen_source_previews(state: &mut State) {
    state.main.screen_source_preview_rx = None;
    state.main.screen_source_preview_inflight = false;
    state.main.screen_source_preview_requested_tab = None;
    state.main.screen_source_preview_last_refresh = None;
}

fn queue_screen_source_preview_refresh(ctx: &egui::Context, state: &mut State) {
    if state.main.screen_source_preview_inflight {
        return;
    }

    let tab = state.main.screen_source_tab;
    let sources = match tab {
        ScreenSourceTab::Applications => state.main.window_sources.clone(),
        ScreenSourceTab::EntireScreen => state.main.screen_sources.clone(),
    };
    if sources.is_empty() {
        state.main.screen_source_preview_requested_tab = Some(tab);
        state.main.screen_source_preview_last_refresh = Some(Instant::now());
        return;
    }

    let (tx, rx) = mpsc::channel();
    let ctx_clone = ctx.clone();
    state.main.screen_source_preview_rx = Some(rx);
    state.main.screen_source_preview_inflight = true;
    state.main.screen_source_preview_requested_tab = Some(tab);

    std::thread::Builder::new()
        .name("screen-source-preview".into())
        .spawn(move || {
            let previews: Vec<ScreenSourcePreviewFrame> = sources
                .into_iter()
                .filter_map(|source| {
                    let target = source.target.clone();
                    crate::voice_livekit::capture_stream_source_preview(
                        &target,
                        SCREEN_SOURCE_PREVIEW_MAX_DIM,
                    )
                    .map(|preview| ScreenSourcePreviewFrame {
                        target,
                        width: preview.width,
                        height: preview.height,
                        rgba: preview.rgba,
                    })
                })
                .collect();
            let _ = tx.send(previews);
            ctx_clone.request_repaint();
        })
        .ok();
}

fn poll_screen_source_preview_updates(ctx: &egui::Context, state: &mut State) {
    let result = match state.main.screen_source_preview_rx.as_ref() {
        Some(rx) => match rx.try_recv() {
            Ok(previews) => Some(Ok(previews)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(())),
        },
        None => None,
    };

    match result {
        Some(Ok(previews)) => {
            state.main.screen_source_preview_rx = None;
            state.main.screen_source_preview_inflight = false;
            state.main.screen_source_preview_last_refresh = Some(Instant::now());

            for preview in previews {
                let size = [preview.width as usize, preview.height as usize];
                let image = egui::ColorImage::from_rgba_unmultiplied(size, &preview.rgba);
                let texture_name = screen_source_texture_name(&preview.target);
                match state.main.screen_source_preview_textures.get_mut(&preview.target) {
                    Some(texture) => {
                        texture.set(image, egui::TextureOptions::LINEAR);
                    }
                    None => {
                        let texture =
                            ctx.load_texture(&texture_name, image, egui::TextureOptions::LINEAR);
                        state
                            .main
                            .screen_source_preview_textures
                            .insert(preview.target, texture);
                    }
                }
            }
        }
        Some(Err(())) => {
            state.main.screen_source_preview_rx = None;
            state.main.screen_source_preview_inflight = false;
            state.main.screen_source_preview_last_refresh = Some(Instant::now());
        }
        None => {}
    }
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
    if state.main.voice.deafened_via_toggle
        && state.main.voice.output_muted
        && state.main.voice.mic_muted != muted
    {
        state.main.voice.deafened_via_toggle = false;
        apply_local_output_muted(ctx, state, engine_tx, false);
    }
    apply_local_mic_muted(ctx, state, api, engine_tx, user_id, muted);
}

fn apply_local_mic_muted(
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
    let deafened = current_voice_deafened(&state.main.voice);
    sync_voice_presence(state, api);
    update_local_mic_flag(state, user_id, muted);
    update_local_deafened_flag(state, user_id, deafened);
    ctx.request_repaint();
}

fn set_local_output_muted(
    ctx: &egui::Context,
    state: &mut State,
    api: &ApiClient,
    engine_tx: Option<&tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: Option<i64>,
    muted: bool,
) {
    state.main.voice.deafened_via_toggle = false;
    apply_local_output_muted(ctx, state, engine_tx, muted);
    update_local_deafened_flag(state, user_id, current_voice_deafened(&state.main.voice));
    sync_voice_presence(state, api);
}

fn apply_local_output_muted(
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
    state.main.voice.deafened_via_toggle = deafened;
    apply_local_output_muted(ctx, state, engine_tx, deafened);
    apply_local_mic_muted(ctx, state, api, engine_tx, user_id, deafened);
}

fn open_screen_share_picker(ctx: &egui::Context, state: &mut State, start_after_pick: bool) {
    refresh_screen_share_sources(state);
    state.main.start_stream_after_source_pick = start_after_pick;
    state.main.screen_source_tab = if state.main.window_sources.is_empty() {
        ScreenSourceTab::EntireScreen
    } else {
        ScreenSourceTab::Applications
    };
    invalidate_screen_source_previews(state);
    state.main.show_screen_source_picker = true;
    queue_screen_source_preview_refresh(ctx, state);
    ctx.request_repaint();
}

fn show_screen_source_tile_grid(
    ui: &mut egui::Ui,
    theme: &Theme,
    sources: &[ScreenSourceEntry],
    preview_textures: &HashMap<StreamSourceTarget, egui::TextureHandle>,
    selected_source: &Option<ScreenSourceEntry>,
    previews_loading: bool,
) -> Option<ScreenSourceEntry> {
    let available_width = ui.available_width().max(1.0);
    let columns = sources.len().max(1).min(SCREEN_SOURCE_TILE_MAX_COLUMNS);
    let total_gap = SCREEN_SOURCE_TILE_GAP * columns.saturating_sub(1) as f32;
    let tile_width = ((available_width - total_gap) / columns as f32).max(1.0);
    let mut picked_source = None;

    for row in sources.chunks(columns) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = SCREEN_SOURCE_TILE_GAP;
            for source in row {
                if show_screen_source_tile(
                    ui,
                    theme,
                    source,
                    preview_textures,
                    selected_source,
                    previews_loading,
                    tile_width,
                ) {
                    picked_source = Some(source.clone());
                }
            }
        });
        ui.add_space(SCREEN_SOURCE_TILE_GAP);
    }

    picked_source
}

fn screen_source_action_button_style(ui: &egui::Ui, button_width: f32) -> (&'static str, f32) {
    let max_text_width = (button_width - 20.0).max(1.0);
    for (label, font_size) in [
        (SCREEN_SOURCE_ACTION_BUTTON_LABEL, 15.0),
        (SCREEN_SOURCE_ACTION_BUTTON_LABEL_SHORT, 14.0),
        (SCREEN_SOURCE_ACTION_BUTTON_LABEL_COMPACT, 13.0),
        (SCREEN_SOURCE_ACTION_BUTTON_LABEL_MINIMAL, 12.0),
    ] {
        let galley = ui.painter().layout(
            label.to_owned(),
            egui::FontId::proportional(font_size),
            egui::Color32::WHITE,
            f32::INFINITY,
        );
        if galley.size().x <= max_text_width {
            return (label, font_size);
        }
    }

    (SCREEN_SOURCE_ACTION_BUTTON_LABEL_MINIMAL, 12.0)
}

fn show_screen_source_tile(
    ui: &mut egui::Ui,
    theme: &Theme,
    source: &ScreenSourceEntry,
    preview_textures: &HashMap<StreamSourceTarget, egui::TextureHandle>,
    selected_source: &Option<ScreenSourceEntry>,
    previews_loading: bool,
    tile_width: f32,
) -> bool {
    let tile_height = tile_width / SCREEN_SOURCE_TILE_ASPECT;
    let tile_size = egui::vec2(tile_width, tile_height + SCREEN_SOURCE_TILE_LABEL_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(tile_size, egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let preview_rect = egui::Rect::from_min_size(rect.min, egui::vec2(tile_width, tile_height));
    let label_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left(), preview_rect.bottom() + 6.0),
        rect.max,
    );
    let rounding = egui::Rounding::same(12.0);
    let is_selected = selected_source
        .as_ref()
        .map(|entry| entry.target == source.target)
        .unwrap_or(false);
    let hovered = ui
        .ctx()
        .pointer_hover_pos()
        .map(|pos| rect.contains(pos))
        .unwrap_or(false);

    ui.painter()
        .rect_filled(preview_rect, rounding, theme.bg_tertiary);

    if let Some(texture) = preview_textures.get(&source.target) {
        let tex_size = texture.size_vec2();
        if tex_size.x > 0.0 && tex_size.y > 0.0 {
            let scale = (preview_rect.width() / tex_size.x).min(preview_rect.height() / tex_size.y);
            let image_size = tex_size * scale;
            let image_rect = egui::Rect::from_center_size(preview_rect.center(), image_size);
            ui.put(
                image_rect,
                egui::Image::new(texture).fit_to_exact_size(image_size),
            );
        }
    } else {
        let placeholder = if previews_loading {
            SCREEN_SOURCE_LABEL_LOADING_PREVIEW
        } else {
            SCREEN_SOURCE_LABEL_PREVIEW_UNAVAILABLE
        };
        ui.painter().text(
            preview_rect.center(),
            egui::Align2::CENTER_CENTER,
            placeholder,
            egui::FontId::proportional(15.0),
            theme.text_muted,
        );
    }

    let stroke = if is_selected {
        egui::Stroke::new(2.0, theme.accent)
    } else if hovered {
        egui::Stroke::new(1.0, theme.border_strong)
    } else {
        egui::Stroke::new(1.0, theme.border)
    };
    ui.painter().rect_stroke(preview_rect, rounding, stroke);

    if hovered {
        ui.painter().rect_filled(
            preview_rect,
            rounding,
            egui::Color32::from_black_alpha(92),
        );
        let button_width = (preview_rect.width() - 24.0)
            .min(220.0)
            .max(48.0)
            .min((preview_rect.width() - 8.0).max(1.0));
        let button_height = if button_width < 112.0 { 34.0 } else { 38.0 };
        let button_size = egui::vec2(button_width, button_height);
        let button_rect = egui::Rect::from_center_size(preview_rect.center(), button_size);
        ui.painter()
            .rect_filled(button_rect, egui::Rounding::same(10.0), theme.accent);
        if button_rect.width() >= 40.0 {
            let (button_label, button_font_size) =
                screen_source_action_button_style(ui, button_rect.width());
            ui.painter().text(
                button_rect.center(),
                egui::Align2::CENTER_CENTER,
                button_label,
                egui::FontId::proportional(button_font_size),
                theme.text_primary,
            );
        }
    }

    ui.allocate_ui_at_rect(label_rect, |ui| {
        ui.with_layout(
            egui::Layout::top_down_justified(egui::Align::Center),
            |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&source.label)
                            .strong()
                            .color(theme.text_primary),
                    )
                    .wrap(),
                );
            },
        );
    });

    response.clicked()
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
    crate::console_panel::poll_console();

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
    let settings_overlay_rect = ctx.screen_rect();
    let settings_window_rect = egui::Rect::from_min_max(
        settings_overlay_rect.min + egui::vec2(100.0, 50.0),
        settings_overlay_rect.max - egui::vec2(100.0, 50.0),
    );
    let settings_window_size = egui::vec2(
        settings_window_rect.width().max(360.0),
        settings_window_rect.height().max(320.0),
    );
    let settings_window_frame = egui::Frame::window(ctx.style().as_ref());
    let settings_window_margin = settings_window_frame.inner_margin;
    let settings_window_border_padding = settings_window_frame.stroke.width / 2.0;
    let mut settings_window_inner_margin = settings_window_margin;
    settings_window_inner_margin += settings_window_border_padding;
    let settings_title_bar_height = egui::TextStyle::Heading
        .resolve(ctx.style().as_ref())
        .size
        .max(ctx.style().spacing.interact_size.y)
        + settings_window_margin.top
        + settings_window_margin.bottom;
    let settings_window_chrome = settings_window_frame.outer_margin.sum()
        + settings_window_inner_margin.sum()
        + egui::vec2(0.0, settings_title_bar_height);
    let settings_window_inner_size = egui::vec2(
        (settings_window_size.x - settings_window_chrome.x).max(0.0),
        (settings_window_size.y - settings_window_chrome.y).max(0.0),
    );
    let settings_scroll_max_height = (settings_window_inner_size.y - 92.0).max(120.0);
    let settings_menu_width = (settings_window_size.x * 0.22).clamp(170.0, 230.0);
    let settings_content_width = (settings_window_size.x - settings_menu_width - 40.0).max(180.0);
    let settings_group_width = (settings_content_width - 20.0).max(160.0);
    let settings_input_width = (settings_group_width - 32.0).max(160.0);
    let server_settings_window_size = egui::vec2(
        settings_window_size.x.min(760.0).max(560.0),
        settings_window_size.y.min(460.0).max(360.0),
    );
    let server_settings_window_rect =
        egui::Rect::from_center_size(settings_overlay_rect.center(), server_settings_window_size);
    let server_settings_scroll_max_height = (server_settings_window_size.y - 150.0).max(120.0);

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
            reset_voice_connection_metrics(state);
            state.main.voice = VoiceState::default();
            apply_persisted_media_preferences(state);
            state.main.voice_video_textures.clear();
            state.main.voice_render_fps.clear();
            state.main.voice_receiver_telemetry = None;
            state.main.fullscreen_stream_user = None;
            state.main.voice_grid_focus_user = None;
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
        let mut should_create_link = false;
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
                if ui.button("Создать ссылку-приглашение").clicked() {
                    should_create_link = true;
                }
                if let Some(ref invite_link) = state.main.invite_link {
                    ui.add_space(6.0);
                    ui.label("Ссылка:");
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(invite_link).monospace().small());
                        if ui.button("Копировать").clicked() {
                            ctx.output_mut(|o| o.copied_text = invite_link.clone());
                        }
                    });
                    ui.add_space(8.0);
                }
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
                        Err(ApiError::Status(403)) => {
                            state.main.invite_msg = Some("Пользователь забанен".to_string());
                        }
                        Err(ApiError::Status(404)) => {
                            state.main.invite_msg =
                                Some("Ошибка: пользователь не найден.".to_string());
                        }
                        Err(ApiError::Status(409)) => {
                            state.main.invite_msg =
                                Some("Пользователь уже находится на сервере.".to_string());
                        }
                        Err(_) => {
                            state.main.invite_msg = Some("Ошибка приглашения.".to_string());
                        }
                    }
                    ctx.request_repaint();
                }
            } else {
                state.main.invite_msg = Some("Введите корректный числовой ID.".to_string());
            }
        }
        if should_create_link {
            if let (Some(server_id), Some(ref token)) =
                (state.main.selected_server, &state.access_token)
            {
                let token = token.clone();
                let channel_id = state.main.selected_channel;
                match block_on(api.create_invite_link(&token, server_id, channel_id)) {
                    Ok(invite) => {
                        state.main.invite_link =
                            Some(format!("{}/invite/{}", api.base, invite.token));
                        state.main.invite_msg = Some("Ссылка-приглашение создана.".to_string());
                    }
                    Err(e) => {
                        state.main.invite_msg = Some(format!("Ошибка создания ссылки: {e}"));
                    }
                }
                ctx.request_repaint();
            }
        }
        if should_close {
            state.main.show_invite_dialog = false;
            state.main.invite_user_id_input.clear();
            state.main.invite_msg = None;
            state.main.invite_link = None;
        }
    }

    if state.main.pending_invite_token.is_some() {
        if state.main.pending_invite_preview.is_none()
            && matches!(state.main.pending_invite_status, LoadState::Idle)
        {
            if let Some(token) = state.main.pending_invite_token.clone() {
                state.main.pending_invite_status = LoadState::Loading;
                match block_on(api.get_invite_preview(&token)) {
                    Ok(preview) => {
                        state.main.pending_invite_preview = Some(preview);
                        state.main.pending_invite_status = LoadState::Loaded;
                        state.main.pending_invite_msg = None;
                    }
                    Err(ApiError::Status(404)) => {
                        state.main.pending_invite_status =
                            LoadState::Error("Приглашение не найдено".to_string());
                    }
                    Err(e) => {
                        state.main.pending_invite_status =
                            LoadState::Error(format!("Ошибка приглашения: {e}"));
                    }
                }
            }
        }

        let mut should_accept = false;
        let mut should_decline = false;
        let server_name = state
            .main
            .pending_invite_preview
            .as_ref()
            .map(|invite| invite.server_name.clone())
            .unwrap_or_else(|| "Приглашение".to_string());

        egui::Window::new("Приглашение в сервер")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.heading(server_name);
                if let Some(invite) = state.main.pending_invite_preview.as_ref() {
                    if let Some(channel_name) = invite.channel_name.as_ref() {
                        ui.label(
                            egui::RichText::new(format!("Канал: {channel_name}"))
                                .small()
                                .weak(),
                        );
                    }
                }
                ui.add_space(8.0);
                match &state.main.pending_invite_status {
                    LoadState::Loading => {
                        ui.label("Загружаем приглашение...");
                    }
                    LoadState::Error(error) => {
                        ui.label(error);
                    }
                    _ => {
                        ui.horizontal(|ui| {
                            if ui.button("Принять").clicked() {
                                should_accept = true;
                            }
                            if ui.button("Отклонить").clicked() {
                                should_decline = true;
                            }
                        });
                    }
                }
                if let Some(ref msg) = state.main.pending_invite_msg {
                    ui.add_space(6.0);
                    ui.label(msg);
                }
            });

        if should_accept {
            if let (Some(ref token), Some(invite_token)) = (
                state.access_token.as_ref(),
                state.main.pending_invite_token.clone(),
            ) {
                let token = token.clone();
                match block_on(api.accept_invite(&token, &invite_token)) {
                    Ok(response) => {
                        if !state
                            .main
                            .servers
                            .iter()
                            .any(|server| server.id == response.server.id)
                        {
                            state.main.servers.push(response.server.clone());
                        }
                        state.main.selected_server = Some(response.server.id);
                        state.main.selected_channel = None;
                        state.main.channels_load_for = None;
                        state.main.channels_load = LoadState::Idle;
                        state.main.retry_channels = true;
                        state.main.pending_invite_token = None;
                        state.main.pending_invite_preview = None;
                        state.main.pending_invite_status = LoadState::Idle;
                        state.main.pending_invite_msg = None;
                    }
                    Err(ApiError::Status(403)) => {
                        state.main.pending_invite_msg = Some("Пользователь забанен".to_string());
                    }
                    Err(e) => {
                        state.main.pending_invite_msg =
                            Some(format!("Ошибка принятия приглашения: {e}"));
                    }
                }
            } else {
                state.main.pending_invite_msg =
                    Some("Сначала войдите в аккаунт, затем примите приглашение.".to_string());
            }
        }

        if should_decline {
            state.main.pending_invite_token = None;
            state.main.pending_invite_preview = None;
            state.main.pending_invite_status = LoadState::Idle;
            state.main.pending_invite_msg = None;
        }
    }

    if state.main.show_server_settings_dialog {
        let mut should_close = false;
        let mut server_settings_open = true;
        let mut do_save_name = false;
        let mut should_reload_bans = false;
        let can_manage_server = selected_server_owner_id(state) == state.user_id;

        show_modal_backdrop(ctx, "server_settings_backdrop", settings_overlay_rect);

        egui::Window::new("Настройки сервера")
            .order(egui::Order::Foreground)
            .open(&mut server_settings_open)
            .collapsible(false)
            .resizable(false)
            .vscroll(true)
            .fixed_pos(server_settings_window_rect.min)
            .fixed_size(server_settings_window_size)
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(170.0);
                        if ui
                            .selectable_label(
                                state.main.server_settings_section
                                    == ServerSettingsSection::General,
                                "Основные",
                            )
                            .clicked()
                        {
                            state.main.server_settings_section = ServerSettingsSection::General;
                        }
                        if ui
                            .selectable_label(
                                state.main.server_settings_section
                                    == ServerSettingsSection::BanList,
                                "Список банов",
                            )
                            .clicked()
                        {
                            state.main.server_settings_section = ServerSettingsSection::BanList;
                            if can_manage_server
                                && matches!(state.main.server_bans_load, LoadState::Idle)
                            {
                                should_reload_bans = true;
                            }
                        }
                    });

                    ui.separator();
                    ui.add_space(12.0);

                    ui.vertical(|ui| match state.main.server_settings_section {
                        ServerSettingsSection::General => {
                            ui.heading("Основные");
                            ui.add_space(8.0);
                            ui.label("Название сервера");
                            ui.add(
                                egui::TextEdit::singleline(
                                    &mut state.main.server_settings_name_input,
                                )
                                .desired_width(340.0),
                            );
                            ui.add_space(10.0);
                            if ui
                                .add_enabled(can_manage_server, egui::Button::new("Сохранить"))
                                .clicked()
                            {
                                do_save_name = true;
                            }
                        }
                        ServerSettingsSection::BanList => {
                            ui.heading("Список банов");
                            ui.add_space(8.0);
                            if !can_manage_server {
                                ui.label(
                                    "Только владелец сервера может просматривать список банов.",
                                );
                            } else {
                                match &state.main.server_bans_load {
                                    LoadState::Loading => {
                                        ui.label("Загружаем список банов...");
                                    }
                                    LoadState::Error(error) => {
                                        ui.label(error);
                                        if ui.button("Повторить").clicked() {
                                            should_reload_bans = true;
                                        }
                                    }
                                    _ => {
                                        let bans_snapshot = state.main.server_bans.clone();
                                        if bans_snapshot.is_empty() {
                                            ui.label("Забаненных пользователей пока нет.");
                                        } else {
                                            egui::ScrollArea::vertical()
                                                .max_height(server_settings_scroll_max_height)
                                                .show(ui, |ui| {
                                                    for banned in &bans_snapshot {
                                                        let label = if banned
                                                            .display_name
                                                            .trim()
                                                            .is_empty()
                                                        {
                                                            banned.username.clone()
                                                        } else {
                                                            format!(
                                                                "{} ({})",
                                                                banned.display_name,
                                                                banned.username
                                                            )
                                                        };
                                                        let response =
                                                            ui.selectable_label(false, label);
                                                        if can_manage_server {
                                                            response.context_menu(|ui| {
                                                                if ui
                                                                    .add(
                                                                        egui::Button::new(
                                                                            "Разбанить",
                                                                        )
                                                                        .fill(theme.success)
                                                                        .stroke(egui::Stroke::NONE),
                                                                    )
                                                                    .clicked()
                                                                {
                                                                    if let (
                                                                        Some(server_id),
                                                                        Some(ref token),
                                                                    ) = (
                                                                        state.main.selected_server,
                                                                        &state.access_token,
                                                                    ) {
                                                                        let token = token.clone();
                                                                        if block_on(
                                                                            api.unban_member(
                                                                                &token,
                                                                                server_id,
                                                                                banned.user_id,
                                                                            ),
                                                                        )
                                                                        .is_ok()
                                                                        {
                                                                            state
                                                                                .main
                                                                                .server_bans
                                                                                .retain(|member| {
                                                                                    member.user_id
                                                                                        != banned
                                                                                            .user_id
                                                                                });
                                                                        }
                                                                    }
                                                                    ui.close_menu();
                                                                }
                                                            });
                                                        }
                                                    }
                                                });
                                        }
                                    }
                                }
                            }
                        }
                    });
                });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if let Some(ref msg) = state.main.server_settings_msg {
                        ui.label(msg);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Закрыть").clicked() {
                            should_close = true;
                        }
                    });
                });
            });

        if !server_settings_open {
            should_close = true;
        }

        if should_reload_bans {
            if !can_manage_server {
                state.main.server_bans.clear();
                state.main.server_bans_load = LoadState::Idle;
            } else if let (Some(server_id), Some(ref token)) =
                (state.main.selected_server, &state.access_token)
            {
                state.main.server_bans_load = LoadState::Loading;
                let token = token.clone();
                match block_on(api.list_server_bans(&token, server_id)) {
                    Ok(bans) => {
                        state.main.server_bans = bans;
                        state.main.server_bans_load = LoadState::Loaded;
                    }
                    Err(e) => {
                        state.main.server_bans_load =
                            LoadState::Error(format!("Ошибка загрузки банов: {e}"));
                    }
                }
            }
        }

        if do_save_name {
            let name = state.main.server_settings_name_input.trim().to_string();
            if !name.is_empty() {
                if let (Some(server_id), Some(ref token)) =
                    (state.main.selected_server, &state.access_token)
                {
                    let token = token.clone();
                    match block_on(api.rename_server(&token, server_id, &name)) {
                        Ok(server) => {
                            if let Some(existing) = state
                                .main
                                .servers
                                .iter_mut()
                                .find(|item| item.id == server.id)
                            {
                                existing.name = server.name.clone();
                            }
                            state.main.server_settings_msg =
                                Some("Название сервера обновлено.".to_string());
                        }
                        Err(e) => {
                            state.main.server_settings_msg =
                                Some(format!("Ошибка сохранения: {e}"));
                        }
                    }
                }
            }
        }

        if should_close {
            state.main.show_server_settings_dialog = false;
            state.main.server_settings_msg = None;
        }
    }

    // ── Dialog: user settings ─────────────────────────────────────────────
    if state.main.show_settings_dialog {
        let mut should_close = false;
        let mut settings_open = true;
        let mut do_nick = false;
        let mut do_avatar = false;
        let mut new_input_vol: Option<f32> = None;
        let mut new_input_sensitivity: Option<f32> = None;
        let mut new_output_vol: Option<f32> = None;
        show_modal_backdrop(ctx, "settings_backdrop", settings_overlay_rect);
        egui::Window::new("Настройки")
            .order(egui::Order::Foreground)
            .open(&mut settings_open)
            .collapsible(false)
            .resizable(false)
            .vscroll(true)
            .fixed_pos(settings_window_rect.min)
            .fixed_size(settings_window_inner_size)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(settings_menu_width);
                            ui.heading("Разделы");
                            ui.add_space(10.0);

                            let account_selected =
                                state.main.settings_section == SettingsSection::Account;
                            if ui
                                .add_sized(
                                    [settings_menu_width - 10.0, 34.0],
                                    egui::SelectableLabel::new(account_selected, "Аккаунт"),
                                )
                                .clicked()
                            {
                                state.main.settings_section = SettingsSection::Account;
                            }

                            let voice_video_selected =
                                state.main.settings_section == SettingsSection::VoiceVideo;
                            if ui
                                .add_sized(
                                    [settings_menu_width - 10.0, 34.0],
                                    egui::SelectableLabel::new(
                                        voice_video_selected,
                                        "Голос и видео",
                                    ),
                                )
                                .clicked()
                            {
                                state.main.settings_section = SettingsSection::VoiceVideo;
                            }

                            let app_selected =
                                state.main.settings_section == SettingsSection::Application;
                            if ui
                                .add_sized(
                                    [settings_menu_width - 10.0, 34.0],
                                    egui::SelectableLabel::new(
                                        app_selected,
                                        "Приложение",
                                    ),
                                )
                                .clicked()
                            {
                                state.main.settings_section = SettingsSection::Application;
                            }
                        });

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(8.0);

                        egui::ScrollArea::vertical()
                            .id_source("settings_dialog_scroll")
                            .max_height(settings_scroll_max_height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_min_width(settings_content_width);
                                ui.vertical(|ui| match state.main.settings_section {
                                SettingsSection::Account => {
                                    ui.heading("Аккаунт");
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "Управляйте тем, как вас видят на сервере.",
                                        )
                                        .small()
                                        .weak(),
                                    );
                                    ui.add_space(12.0);

                                    ui.group(|ui| {
                                        ui.set_min_width(settings_group_width);
                                        ui.label(egui::RichText::new("Смена никнейма").strong());
                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new(
                                                "Никнейм меняется только для текущего сервера.",
                                            )
                                            .small()
                                            .weak(),
                                        );
                                        ui.add_space(8.0);
                                        ui.add(
                                            egui::TextEdit::singleline(
                                                &mut state.main.settings_nickname_input,
                                            )
                                            .hint_text("Ваш ник")
                                            .desired_width(settings_input_width),
                                        );
                                        ui.add_space(8.0);
                                        if ui.button("Сохранить никнейм").clicked() {
                                            do_nick = true;
                                        }
                                    });

                                    ui.add_space(10.0);

                                    ui.group(|ui| {
                                        ui.set_min_width(settings_group_width);
                                        ui.label(egui::RichText::new("Изменение аватарки").strong());
                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new(
                                                "Поддерживаются PNG, JPG, GIF и WebP.",
                                            )
                                            .small()
                                            .weak(),
                                        );
                                        ui.add_space(8.0);
                                        if ui.button("Выбрать файл...").clicked() {
                                            do_avatar = true;
                                        }
                                        if let Some(ref p) = state.main.settings_avatar_path {
                                            ui.add_space(6.0);
                                            ui.label(
                                                egui::RichText::new(p.display().to_string())
                                                    .small()
                                                    .weak(),
                                            );
                                        }
                                    });
                                }
                                SettingsSection::VoiceVideo => {
                                    ui.heading("Голос и видео");
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "Настройки захвата, шумоподавления и воспроизведения.",
                                        )
                                        .small()
                                        .weak(),
                                    );
                                    ui.add_space(12.0);

                                    ui.group(|ui| {
                                        ui.set_min_width(settings_group_width);
                                        ui.heading("Голос");
                                        ui.add_space(8.0);
                ui.label(egui::RichText::new("Громкость микрофона:").small());
                let mut iv = state.main.voice.input_volume;
                let slider_in = egui::Slider::new(&mut iv, 0.0_f32..=2.0_f32)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .show_value(true);
                if ui.add(slider_in).changed() {
                    iv = normalize_input_volume(iv);
                    state.settings.input_volume = iv;
                    state.settings.save();
                    new_input_vol = Some(iv);
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Чувствительность голосового ввода:").small());
                let mut input_sensitivity = state.main.voice.input_sensitivity;
                let sensitivity_slider =
                    egui::Slider::new(&mut input_sensitivity, 0.0_f32..=1.0_f32)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .show_value(true);
                if ui.add(sensitivity_slider).changed() {
                    let input_sensitivity = normalize_input_sensitivity(input_sensitivity);
                    state.settings.input_sensitivity = input_sensitivity;
                    state.settings.save();
                    new_input_sensitivity = Some(input_sensitivity);
                }
                ui.label(
                    egui::RichText::new(
                        "Работает после шумодава; если шумодав выключен, действует как общий голосовой gate.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Громкость динамиков:").small());
                let mut ov = state.main.voice.output_volume;
                let slider_out = egui::Slider::new(&mut ov, 0.0_f32..=4.0_f32)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                    .show_value(true);
                if ui.add(slider_out).changed() {
                    ov = normalize_output_volume(ov);
                    state.settings.output_volume = ov;
                    state.settings.save();
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
                                    });

                                    ui.add_space(10.0);

                                    ui.group(|ui| {
                                        ui.set_min_width(settings_group_width);
                                        ui.heading("Видео");
                                        ui.add_space(8.0);
                                        ui.label(
                                            egui::RichText::new(
                                                "Декодирование входящего видео:",
                                            )
                                            .small(),
                                        );
                                        let mut dp: String = state.settings.decode_path.clone();
                                        egui::ComboBox::from_id_source("decode_path")
                                            .selected_text(match dp.as_str() {
                                                "nvenc" => "NVIDIA (NVENC)".to_string(),
                                                "mft" => "MFT (Media Foundation)".to_string(),
                                                _ => "CPU (OpenH264)".to_string(),
                                            })
                                            .show_ui(ui, |ui| {
                                                let _ = ui.selectable_value(
                                                    &mut dp,
                                                    "cpu".to_string(),
                                                    "CPU (OpenH264)",
                                                );
                                                let _ = ui.selectable_value(
                                                    &mut dp,
                                                    "mft".to_string(),
                                                    "MFT (Media Foundation)",
                                                );
                                                let _ = ui.selectable_value(
                                                    &mut dp,
                                                    "nvenc".to_string(),
                                                    "NVIDIA (NVENC)",
                                                );
                                            });
                                        if dp != state.settings.decode_path {
                                            state.settings.decode_path = dp;
                                            state.settings.save();
                                        }

                                        ui.add_space(6.0);
                                        ui.label(
                                            egui::RichText::new(
                                                "Ползунок гаммы удалён: цветокоррекция декодера больше не используется.",
                                            )
                                            .small()
                                            .weak(),
                                        );
                                    });
                                }
                                SettingsSection::Application => {
                                    ui.heading("Приложение");
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "Настройки приложения и инструменты разработки.",
                                        )
                                        .small()
                                        .weak(),
                                    );
                                    ui.add_space(12.0);

                                    ui.group(|ui| {
                                        ui.set_min_width(settings_group_width);
                                        ui.label(egui::RichText::new("Консоль").strong());
                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new(
                                                "Просмотр вывода консоли приложения.",
                                            )
                                            .small()
                                            .weak(),
                                        );
                                        ui.add_space(8.0);
                                        if ui.button("Открыть консоль").clicked() {
                                            state.main.show_console_dialog = true;
                                        }
                                    });
                                }
                                });
                            });
                    });

                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if let Some(ref msg) = state.main.settings_msg {
                            ui.label(msg);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Закрыть").clicked() {
                                should_close = true;
                            }
                        });
                    });
                });
            });
        if !settings_open {
            should_close = true;
        }

        if let Some(v) = new_input_vol {
            state.main.voice.input_volume = v;
            if let Some(tx) = engine_tx.as_ref() {
                tx.send(VoiceCmd::SetInputVolume(v)).ok();
            }
        }
        if let Some(v) = new_input_sensitivity {
            state.main.voice.input_sensitivity = v;
            if let Some(tx) = engine_tx.as_ref() {
                tx.send(VoiceCmd::SetInputSensitivity(v)).ok();
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
            state.main.settings_avatar_path = None;
            state.main.settings_msg = None;
            state.settings.save();
        }
    }

    // ── Dialog: console window ───────────────────────────────────────────
    if state.main.show_console_dialog {
        // Poll for new console messages
        crate::console_panel::poll_console();

        let mut console_open = true;
        let console_rect = egui::Rect::from_min_size(
            settings_overlay_rect.min + egui::vec2(50.0, 50.0),
            egui::vec2(700.0, 500.0),
        );

        show_modal_backdrop(ctx, "console_backdrop", settings_overlay_rect);

        egui::Window::new("Консоль")
            .order(egui::Order::Foreground)
            .open(&mut console_open)
            .collapsible(true)
            .resizable(true)
            .fixed_pos(console_rect.min)
            .fixed_size(egui::vec2(700.0, 500.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Вывод консоли");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let panel = crate::console_panel::get_console_panel().lock();
                        let lines = panel.lines();
                        let copy_text = lines
                            .iter()
                            .map(|l| format!("[{}] {}", l.timestamp, l.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        drop(panel);

                        if ui.button("Копировать").clicked() {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                let _ = clipboard.set_text(&copy_text);
                            }
                        }
                        if ui.button("Очистить").clicked() {
                            crate::console_panel::get_console_panel().lock().clear();
                        }
                    });
                });
                ui.separator();

                let panel = crate::console_panel::get_console_panel().lock();
                let lines = panel.lines().to_vec();
                drop(panel);

                egui::ScrollArea::vertical()
                    .id_source("console_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in lines {
                            let color = if line.is_stderr {
                                egui::Color32::RED
                            } else {
                                egui::Color32::GRAY
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "[{}] {}",
                                    line.timestamp,
                                    line.text
                                ))
                                .monospace()
                                .color(color),
                            );
                        }
                    });
            });

        if !console_open {
            state.main.show_console_dialog = false;
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
    sample_voice_latency_history(state);
    let left_user_panel_height = bottom_panel::panel_height(state.main.voice.channel_id.is_some());
    if state.main.voice.channel_id.is_some() {
        ctx.request_repaint_after(Duration::from_millis(50));
    }

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
                    state.main.chat_search_popup_open = false;
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
                    set_local_output_muted(ctx, state, api, engine_tx.as_ref(), user_id, muted);
                }
                ChannelPanelAction::SetParticipantMuted { user_id, muted } => {
                    set_local_participant_muted(state, engine_tx, user_id, muted);
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
                    state.main.invite_link = None;
                }
                ChannelPanelAction::ChannelSettings(id, name) => {
                    state.main.channel_rename = Some((id, name));
                }
                ChannelPanelAction::OpenServerSettings => {
                    open_server_settings(state);
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
        let self_speaking = state
            .user_id
            .and_then(|id| state.main.voice.speaking.lock().get(&id).copied())
            .unwrap_or(false);
        let voice_session_stats = state
            .main
            .voice_stats
            .as_ref()
            .map(|stats| stats.lock().clone())
            .unwrap_or_default();
        let current_latency_ms = voice_session_stats.latency_rtt_ms;
        let voice_connection_label = match (state.main.voice.server_id, state.main.voice.channel_id)
        {
            (Some(server_id), Some(channel_id)) => {
                let server_name = state
                    .main
                    .servers
                    .iter()
                    .find(|server| server.id == server_id)
                    .map(|server| server.name.clone())
                    .unwrap_or_else(|| format!("Сервер {}", server_id));
                let channel_name = state
                    .main
                    .channels
                    .iter()
                    .find(|channel| channel.id == channel_id && channel.server_id == server_id)
                    .map(|channel| channel.name.clone())
                    .unwrap_or_else(|| format!("Голос {}", channel_id));
                Some(format!("{server_name} / {channel_name}"))
            }
            _ => None,
        };
        let voice_bar_snapshot = BottomPanelVoiceSnapshot {
            in_voice_channel: state.main.voice.channel_id.is_some(),
            mic_muted: state.main.voice.mic_muted,
            output_muted: state.main.voice.output_muted,
            screen_on: state.main.voice.screen_on,
            screen_preset: state.main.screen_preset,
            speaking: self_speaking,
            latency_ms: current_latency_ms,
            latency_history_ms: state
                .main
                .voice_latency_history
                .iter()
                .map(|sample| sample.latency_ms)
                .collect(),
            screen_audio_muted: state.main.voice.screen_audio_muted,
            voice_connection_label,
            stream_fps: voice_session_stats
                .stream_fps
                .or(voice_session_stats.frames_per_second),
            resolution: voice_session_stats.resolution,
            outgoing_speed_mbps: voice_session_stats.connection_speed_mbps,
            webrtc_requested_bitrate_mbps: voice_session_stats.webrtc_requested_bitrate_mbps,
            webrtc_target_bitrate_mbps: voice_session_stats.webrtc_target_bitrate_mbps,
            webrtc_fps_hint: voice_session_stats.webrtc_fps_hint,
            encoded_pre_rtp_bitrate_mbps: voice_session_stats.encoded_pre_rtp_bitrate_mbps,
            source_fps: voice_session_stats.source_fps,
            source_cap_fps: voice_session_stats.source_cap_fps,
            webrtc_effective_fps_cap: voice_session_stats.webrtc_effective_fps_cap,
            startup_transport_cap_fps: voice_session_stats.startup_transport_cap_fps,
            final_schedule_cap_fps: voice_session_stats.final_schedule_cap_fps,
            webrtc_available_outgoing_bitrate_mbps: voice_session_stats
                .webrtc_available_outgoing_bitrate_mbps,
            webrtc_packet_loss_pct: voice_session_stats.webrtc_packet_loss_pct,
            webrtc_nack_count: voice_session_stats.webrtc_nack_count,
            webrtc_pli_count: voice_session_stats.webrtc_pli_count,
            webrtc_quality_limitation_reason: voice_session_stats
                .webrtc_quality_limitation_reason
                .clone(),
            webrtc_transport_path: voice_session_stats.webrtc_transport_path.clone(),
            webrtc_transport_rtt_ms: voice_session_stats.webrtc_transport_rtt_ms,
            encoding_path: voice_session_stats.encoding_path.clone(),
            decoding_path: voice_session_stats.decoding_path.clone(),
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
                                user_id: state.user_id,
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
                BottomPanelAction::SetScreenAudioMuted(muted) => {
                    state.main.voice.screen_audio_muted = muted;
                    if let Some(tx) = engine_tx.as_ref() {
                        tx.send(VoiceCmd::SetScreenAudioMuted(muted)).ok();
                    }
                    ctx.request_repaint();
                }
                BottomPanelAction::OpenSettings => {
                    state.main.show_settings_dialog = true;
                    state.main.settings_section = SettingsSection::Account;
                    state.main.settings_nickname_input = state.main.my_display_name.clone();
                    state.main.settings_avatar_path = None;
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
                    state.settings.screen_preset = preset;
                    state.settings.save();
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
        let can_moderate_members = state.user_id == Some(server_owner_id);

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
                        current_user_id: state.user_id,
                        can_moderate: can_moderate_members,
                        on_action: &mut |action| member_actions.push(action),
                    },
                );
            });

        for action in member_actions {
            match action {
                MemberPanelAction::OpenMemberProfile(user_id) => {
                    todo_actions::todo_open_member_profile(user_id);
                }
                MemberPanelAction::KickMember(user_id) => {
                    kick_server_member(ctx, state, api, user_id);
                }
                MemberPanelAction::BanMember(user_id) => {
                    ban_server_member(ctx, state, api, user_id);
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

            let ch_name: String = state
                .main
                .channels
                .iter()
                .find(|c| Some(c.id) == state.main.selected_channel)
                .map(|c| {
                    if c.r#type == "text" {
                        format!("# {}", c.name)
                    } else {
                        format!("\u{1F50A} {}", c.name)
                    }
                })
                .unwrap_or_default();

            let mut chat_actions: Vec<ChatPanelAction> = Vec::new();
            let mut pick_file = false;

            if is_text_channel {
                let typing_users: Vec<(i64, String)> = state
                    .main
                    .typing_users
                    .iter()
                    .map(|(id, name, _)| (*id, name.clone()))
                    .collect();
                let messages_loading = state.main.messages_load == LoadState::Loading;
                let messages_load_error = match &state.main.messages_load {
                    LoadState::Error(s) => Some(s.clone()),
                    _ => None,
                };
                let search_loading = state.main.chat_search_load == LoadState::Loading;
                let search_error = match &state.main.chat_search_load {
                    LoadState::Error(s) => Some(s.as_str()),
                    _ => None,
                };
                let highlighted_message_id =
                    if state.main.highlighted_message_channel_id == state.main.selected_channel {
                        state.main.highlighted_message_id
                    } else {
                        None
                    };
                let highlighted_message_t =
                    state.main.highlighted_message_until.and_then(|until| {
                        let remaining = until.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            None
                        } else {
                            Some((remaining.as_secs_f32() / 3.0).clamp(0.0, 1.0))
                        }
                    });
                let can_moderate_members = selected_server_owner_id(state) == state.user_id;
                chat_panel::show(
                    ctx,
                    ui,
                    ChatPanelParams {
                        theme,
                        channel_name: &ch_name,
                        channel_description: None,
                        messages: &state.main.messages,
                        search_query: &mut state.main.chat_search_query,
                        search_popup_open: &mut state.main.chat_search_popup_open,
                        search_results: &state.main.chat_search_results,
                        search_scroll_offset: &mut state.main.chat_search_scroll_offset,
                        search_loading,
                        search_error,
                        highlighted_message_id,
                        highlighted_message_t,
                        scroll_to_highlighted: &mut state.main.highlighted_message_scroll_pending,
                        new_message: &mut state.main.new_message,
                        typing_users: &typing_users,
                        pending_attachment: state.main.pending_attachment.as_ref(),
                        current_user_id: state.user_id,
                        can_moderate_members,
                        server_members: &state.main.server_members,
                        media_textures,
                        media_bytes,
                        avatar_textures,
                        on_action: &mut |a| chat_actions.push(a),
                        messages_load_error,
                        messages_loading,
                    },
                );

                for act in chat_actions {
                    let should_close_search_popup = !matches!(&act, ChatPanelAction::Search);
                    if should_close_search_popup {
                        state.main.chat_search_popup_open = false;
                    }
                    match act {
                        ChatPanelAction::SendMessage => {
                            let text = state.main.new_message.trim().to_string();
                            let has_attachment = state.main.pending_attachment.is_some();
                            if !text.is_empty() || has_attachment {
                                if let (Some(ch_id), Some(ref token)) =
                                    (state.main.selected_channel, &state.access_token)
                                {
                                    let token = token.clone();
                                    let attachments: Vec<AttachmentMeta> =
                                        state.main.pending_attachment.take().into_iter().collect();
                                    let content = if text.is_empty() {
                                        attachments
                                            .first()
                                            .map(|a| a.filename.clone())
                                            .unwrap_or_default()
                                    } else {
                                        text
                                    };
                                    state.main.pending_attachment_bytes = None;
                                    if let Ok(msg) = block_on(api.send_message(
                                        &token,
                                        ch_id,
                                        &content,
                                        attachments,
                                    )) {
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
                        ChatPanelAction::Notifications => todo_actions::todo_open_notifications(),
                        ChatPanelAction::Pinned => todo_actions::todo_open_pins(),
                        ChatPanelAction::Search => todo_actions::todo_search_messages(),
                        ChatPanelAction::Inbox => todo_actions::todo_open_inbox(),
                        ChatPanelAction::Help => todo_actions::todo_open_help(),
                        ChatPanelAction::ToggleMemberList => {
                            let current = state.main.show_member_panel.unwrap_or(true);
                            state.main.show_member_panel = Some(!current);
                            ctx.request_repaint();
                        }
                        ChatPanelAction::OpenSearchResult {
                            channel_id,
                            message_id,
                        } => {
                            let same_channel = state.main.selected_channel == Some(channel_id);
                            state.main.selected_channel = Some(channel_id);
                            state.main.unread_channels.remove(&channel_id);
                            state.main.highlighted_message_channel_id = Some(channel_id);
                            state.main.highlighted_message_id = Some(message_id);
                            state.main.highlighted_message_until =
                                Some(Instant::now() + Duration::from_secs(3));
                            state.main.highlighted_message_scroll_pending = true;
                            if same_channel {
                                state.main.retry_messages = true;
                            }
                            ctx.request_repaint();
                        }
                        ChatPanelAction::KickMember(user_id) => {
                            kick_server_member(ctx, state, api, user_id);
                        }
                        ChatPanelAction::BanMember(user_id) => {
                            ban_server_member(ctx, state, api, user_id);
                        }
                        ChatPanelAction::StubGif => todo_actions::todo_insert_gif(),
                        ChatPanelAction::StubEmoji => todo_actions::todo_open_emoji_picker(),
                        ChatPanelAction::StubStickers => todo_actions::todo_open_sticker_picker(),
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
                        let filename = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file")
                            .to_string();
                        let mime = mime_from_path(&path);
                        if let (Some(server_id), Some(ref token)) =
                            (state.main.selected_server, &state.access_token)
                        {
                            let token = token.clone();
                            match block_on(api.upload_media(
                                &token,
                                server_id,
                                &filename,
                                &mime,
                                bytes.clone(),
                            )) {
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
                    state
                        .main
                        .selected_channel
                        .and_then(|ch_id| state.main.channel_voice.get(&ch_id).cloned())
                        .unwrap_or_default()
                };
                let speaking_snap: HashMap<i64, bool> = state.main.voice.speaking.lock().clone();

                if in_this_voice {
                    ctx.request_repaint_after(std::time::Duration::from_millis(80));
                }
                let voice_viewport = ui.available_rect_before_wrap();
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
                    show_voice_grid(
                        ui,
                        theme,
                        state,
                        api,
                        engine_tx,
                        &participants,
                        &speaking_snap,
                        in_this_voice,
                    );
                    if false {
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
                                                let is_speaking = *speaking_snap
                                                    .get(&p.user_id)
                                                    .unwrap_or(&false);
                                                let is_locally_muted = in_this_voice
                                                    && state
                                                        .main
                                                        .voice
                                                        .locally_muted
                                                        .contains(&p.user_id);
                                                let is_stream_subscribed = in_this_voice
                                                    && state
                                                        .main
                                                        .voice
                                                        .stream_subscriptions
                                                        .contains(&p.user_id);
                                                let show_stream_preview = p.streaming
                                                    && in_this_voice
                                                    && !is_stream_subscribed;

                                                // Background (rounded rect)
                                                let fill = ui.visuals().faint_bg_color;
                                                ui.painter().rect_filled(
                                                    rect,
                                                    egui::Rounding::same(ROUNDING),
                                                    fill,
                                                );

                                                // Avatar/video area: prefer stream texture when p.streaming
                                                let content_margin = 8.0;
                                                let avatar_rect = rect.shrink2(egui::vec2(
                                                    content_margin,
                                                    content_margin,
                                                ));
                                                let avatar_rect = egui::Rect::from_min_max(
                                                    avatar_rect.min,
                                                    egui::pos2(
                                                        avatar_rect.max.x,
                                                        avatar_rect.min.y
                                                            + (avatar_rect.height() * 0.72),
                                                    ),
                                                );
                                                let stream_key = video_frame_key(p.user_id, true);
                                                let stream_preview_key =
                                                    video_preview_frame_key(p.user_id);
                                                let camera_key = p.user_id;
                                                let has_stream_texture = state
                                                    .main
                                                    .voice_video_gpu_textures
                                                    .contains_key(&stream_key)
                                                    || state
                                                        .main
                                                        .voice_video_textures
                                                        .contains_key(&stream_key);
                                                let has_stream_preview_texture = state
                                                    .main
                                                    .voice_video_gpu_textures
                                                    .contains_key(&stream_preview_key)
                                                    || state
                                                        .main
                                                        .voice_video_textures
                                                        .contains_key(&stream_preview_key);
                                                let has_camera_texture = state
                                                    .main
                                                    .voice_video_gpu_textures
                                                    .contains_key(&camera_key)
                                                    || state
                                                        .main
                                                        .voice_video_textures
                                                        .contains_key(&camera_key);
                                                let show_stream_connecting = p.streaming
                                                    && in_this_voice
                                                    && is_stream_subscribed
                                                    && !has_stream_texture;
                                                let show_stream_controls = p.streaming
                                                    && in_this_voice
                                                    && is_stream_subscribed
                                                    && has_stream_texture;
                                                let tex_key = if p.streaming {
                                                    if has_stream_texture {
                                                        Some(stream_key)
                                                    } else if has_stream_preview_texture
                                                        && !is_stream_subscribed
                                                    {
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
                                                    let rendered =
                                                        if let Some(&(_, gl_tex_id, _, _)) = state
                                                            .main
                                                            .voice_video_gpu_textures
                                                            .get(&key)
                                                        {
                                                            paint_wgl_video_texture(
                                                                ui,
                                                                avatar_rect,
                                                                gl_tex_id,
                                                            );
                                                            true
                                                        } else {
                                                            false
                                                        };
                                                    if !rendered {
                                                        if let Some(tex) = state
                                                            .main
                                                            .voice_video_textures
                                                            .get(&key)
                                                        {
                                                            let size = avatar_rect.size();
                                                            ui.put(
                                                                avatar_rect,
                                                                egui::Image::new(tex)
                                                                    .fit_to_exact_size(size),
                                                            );
                                                        }
                                                    }
                                                    if is_speaking && !p.streaming {
                                                        ui.painter().rect_stroke(
                                                            avatar_rect.expand(2.0),
                                                            egui::Rounding::same(ROUNDING + 2.0),
                                                            egui::Stroke::new(
                                                                2.0,
                                                                egui::Color32::from_rgb(
                                                                    67, 181, 129,
                                                                ),
                                                            ),
                                                        );
                                                    }
                                                    // Hover overlay: show nickname and LIVE badge on thumbnail when hovered
                                                    let is_hovered = resp.hovered();
                                                    if is_hovered && p.streaming {
                                                        ui.painter().rect_filled(
                                                            avatar_rect,
                                                            egui::Rounding::same(ROUNDING),
                                                            egui::Color32::from_black_alpha(140),
                                                        );
                                                        // LIVE badge
                                                        let live_badge_rect = egui::Rect::from_min_size(
                                                            egui::pos2(avatar_rect.left() + 6.0, avatar_rect.top() + 6.0),
                                                            egui::vec2(40.0, 18.0),
                                                        );
                                                        ui.painter().rect_filled(
                                                            live_badge_rect,
                                                            egui::Rounding::same(4.0),
                                                            egui::Color32::from_rgb(255, 80, 80),
                                                        );
                                                        ui.painter().text(
                                                            live_badge_rect.center(),
                                                            egui::Align2::CENTER_CENTER,
                                                            "LIVE",
                                                            egui::FontId::proportional(10.0),
                                                            egui::Color32::WHITE,
                                                        );
                                                        // Nickname
                                                        let name_text: String = if p.username.is_empty() { "Гость".to_string() } else { p.username.clone() };
                                                        let galley = ui.painter().layout(
                                                            name_text,
                                                            egui::FontId::proportional(14.0),
                                                            egui::Color32::WHITE,
                                                            f32::INFINITY,
                                                        );
                                                        let name_pos = egui::pos2(
                                                            avatar_rect.center().x - galley.size().x / 2.0,
                                                            avatar_rect.center().y + 4.0,
                                                        );
                                                        ui.painter().galley(name_pos, galley, egui::Color32::WHITE);
                                                    }
                                                    if show_stream_preview {
                                                        ui.painter().rect_filled(
                                                            avatar_rect,
                                                            egui::Rounding::same(ROUNDING),
                                                            egui::Color32::from_black_alpha(120),
                                                        );
                                                        let watch_rect =
                                                            egui::Rect::from_center_size(
                                                                avatar_rect.center(),
                                                                egui::vec2(120.0, 34.0),
                                                            );
                                                        ui.allocate_ui_at_rect(watch_rect, |ui| {
                                                            if ui.button("Смотреть").clicked()
                                                            {
                                                                set_stream_subscription(
                                                                    state, engine_tx, p.user_id,
                                                                    true,
                                                                );
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
                                                    // Stream tile: overlay with fullscreen + mute + disconnect in corner
                                                    if show_stream_controls {
                                                        let corner =
                                                            avatar_rect.max - egui::vec2(4.0, 4.0);
                                                        let btn_size = egui::vec2(28.0, 28.0);
                                                        let btn_gap = 4.0;
                                                        let disconnect_rect =
                                                            egui::Rect::from_min_size(
                                                                corner
                                                                    - egui::vec2(
                                                                        btn_size.x * 3.0 + btn_gap * 2.0,
                                                                        0.0,
                                                                    ),
                                                                btn_size,
                                                            );
                                                        let fullscreen_rect =
                                                            egui::Rect::from_min_size(
                                                                corner
                                                                    - egui::vec2(
                                                                        btn_size.x * 2.0 + btn_gap,
                                                                        0.0,
                                                                    ),
                                                                btn_size,
                                                            );
                                                        let mute_rect = egui::Rect::from_min_size(
                                                            corner - egui::vec2(btn_size.x, 0.0),
                                                            btn_size,
                                                        );
                                                        ui.allocate_ui_at_rect(
                                                            disconnect_rect,
                                                            |ui| {
                                                                if ui
                                                                    .button("✕")
                                                                    .on_hover_text("Отключиться от трансляции")
                                                                    .clicked()
                                                                {
                                                                    set_stream_subscription(
                                                                        state, engine_tx, p.user_id,
                                                                        false,
                                                                    );
                                                                }
                                                            },
                                                        );
                                                        ui.allocate_ui_at_rect(
                                                            fullscreen_rect,
                                                            |ui| {
                                                                if ui
                                                                    .button("⛶")
                                                                    .on_hover_text("На весь экран")
                                                                    .clicked()
                                                                {
                                                                    state
                                                                        .main
                                                                        .fullscreen_stream_user =
                                                                        Some(p.user_id);
                                                                }
                                                            },
                                                        );
                                                        ui.allocate_ui_at_rect(mute_rect, |ui| {
                                                            show_stream_audio_button(
                                                                ui, state, engine_tx, p.user_id,
                                                            );
                                                        });
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
                                                            egui::Stroke::new(
                                                                2.0,
                                                                egui::Color32::from_rgb(
                                                                    67, 181, 129,
                                                                ),
                                                            ),
                                                        );
                                                    }
                                                    let letter = p
                                                        .username
                                                        .chars()
                                                        .next()
                                                        .map(|c| c.to_uppercase().to_string())
                                                        .unwrap_or_else(|| "?".to_string());
                                                    let font_size =
                                                        (avatar_rect.height() * 0.4).max(14.0);
                                                    let galley = ui.painter().layout(
                                                        letter,
                                                        egui::FontId::proportional(font_size),
                                                        egui::Color32::WHITE,
                                                        f32::INFINITY,
                                                    );
                                                    let pos =
                                                        avatar_rect.center() - galley.size() / 2.0;
                                                    ui.painter().galley(
                                                        pos,
                                                        galley,
                                                        egui::Color32::WHITE,
                                                    );
                                                    // Hover overlay: show nickname and LIVE badge on thumbnail when hovered
                                                    let is_hovered = resp.hovered();
                                                    if is_hovered && p.streaming {
                                                        ui.painter().rect_filled(
                                                            avatar_rect,
                                                            egui::Rounding::same(ROUNDING),
                                                            egui::Color32::from_black_alpha(140),
                                                        );
                                                        // LIVE badge
                                                        let live_badge_rect = egui::Rect::from_min_size(
                                                            egui::pos2(avatar_rect.left() + 6.0, avatar_rect.top() + 6.0),
                                                            egui::vec2(40.0, 18.0),
                                                        );
                                                        ui.painter().rect_filled(
                                                            live_badge_rect,
                                                            egui::Rounding::same(4.0),
                                                            egui::Color32::from_rgb(255, 80, 80),
                                                        );
                                                        ui.painter().text(
                                                            live_badge_rect.center(),
                                                            egui::Align2::CENTER_CENTER,
                                                            "LIVE",
                                                            egui::FontId::proportional(10.0),
                                                            egui::Color32::WHITE,
                                                        );
                                                        // Nickname
                                                        let name_text: String = if p.username.is_empty() { "Гость".to_string() } else { p.username.clone() };
                                                        let galley = ui.painter().layout(
                                                            name_text,
                                                            egui::FontId::proportional(14.0),
                                                            egui::Color32::WHITE,
                                                            f32::INFINITY,
                                                        );
                                                        let name_pos = egui::pos2(
                                                            avatar_rect.center().x - galley.size().x / 2.0,
                                                            avatar_rect.center().y + 4.0,
                                                        );
                                                        ui.painter().galley(name_pos, galley, egui::Color32::WHITE);
                                                    }
                                                    if show_stream_preview {
                                                        ui.painter().rect_filled(
                                                            avatar_rect,
                                                            egui::Rounding::same(ROUNDING),
                                                            egui::Color32::from_black_alpha(120),
                                                        );
                                                        let watch_rect =
                                                            egui::Rect::from_center_size(
                                                                avatar_rect.center(),
                                                                egui::vec2(120.0, 34.0),
                                                            );
                                                        ui.allocate_ui_at_rect(watch_rect, |ui| {
                                                            if ui.button("Смотреть").clicked()
                                                            {
                                                                set_stream_subscription(
                                                                    state, engine_tx, p.user_id,
                                                                    true,
                                                                );
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
                                                    rect.left_top()
                                                        + egui::vec2(content_margin, name_y),
                                                    rect.right_top()
                                                        + egui::vec2(
                                                            -content_margin,
                                                            name_y + 18.0,
                                                        ),
                                                );
                                                let color = if is_locally_muted {
                                                    ui.visuals().weak_text_color()
                                                } else {
                                                    ui.visuals().text_color()
                                                };
                                                let name = p.username.as_str();
                                                let max_chars = (tile_w / 7.0).max(8.0) as usize;
                                                let trunc = if name.len() > max_chars {
                                                    format!("{}…", &name[..max_chars])
                                                } else {
                                                    name.to_string()
                                                };
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
                                                    ui.painter().text(
                                                        egui::pos2(icon_x, icon_y),
                                                        egui::Align2::LEFT_TOP,
                                                        "🔇",
                                                        egui::FontId::proportional(10.0),
                                                        color,
                                                    );
                                                    icon_x += 14.0;
                                                }
                                                if p.cam_enabled {
                                                    ui.painter().text(
                                                        egui::pos2(icon_x, icon_y),
                                                        egui::Align2::LEFT_TOP,
                                                        "📷",
                                                        egui::FontId::proportional(10.0),
                                                        color,
                                                    );
                                                    icon_x += 14.0;
                                                }
                                                if p.streaming {
                                                    ui.painter().text(
                                                        egui::pos2(icon_x, icon_y),
                                                        egui::Align2::LEFT_TOP,
                                                        "📺",
                                                        egui::FontId::proportional(10.0),
                                                        color,
                                                    );
                                                }

                                                if in_this_voice && Some(p.user_id) != state.user_id {
                                                    resp.context_menu(|ui| {
                                                        let mute_label = if is_locally_muted {
                                                            "Снять локальный мут"
                                                        } else {
                                                            "Заглушить локально"
                                                        };
                                                        if ui.button(mute_label).clicked() {
                                                            if is_locally_muted {
                                                                state
                                                                    .main
                                                                    .voice
                                                                    .locally_muted
                                                                    .remove(&p.user_id);
                                                            } else {
                                                                state
                                                                    .main
                                                                    .voice
                                                                    .locally_muted
                                                                    .insert(p.user_id);
                                                            }
                                                            sync_user_volume(
                                                                engine_tx,
                                                                &state.main.voice,
                                                                p.user_id,
                                                            );
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
                                                        ui.label(
                                                            "Громкость 0–300%, по умолчанию 100%",
                                                        );
                                                        let uid = p.user_id;
                                                        let mut vol = *state
                                                            .main
                                                            .voice
                                                            .local_volumes
                                                            .get(&uid)
                                                            .unwrap_or(&1.0);
                                                        if ui
                                                            .add(
                                                                egui::Slider::new(
                                                                    &mut vol,
                                                                    0.0..=3.0,
                                                                )
                                                                .step_by(0.01)
                                                                .custom_formatter(|v, _| {
                                                                    format!("{:.0}%", v * 100.0)
                                                                })
                                                                .text(""),
                                                            )
                                                            .changed()
                                                        {
                                                            state
                                                                .main
                                                                .voice
                                                                .local_volumes
                                                                .insert(uid, vol);
                                                            state
                                                                .settings
                                                                .voice_volume_by_user
                                                                .insert(uid.to_string(), vol);
                                                            state.settings.save();
                                                            sync_user_volume(
                                                                engine_tx,
                                                                &state.main.voice,
                                                                uid,
                                                            );
                                                        }
                                                    });
                                                }
                                            });
                                        }
                                    });
                                    ui.add_space(4.0);
                                }
                            });
                    }
                }
                show_voice_grid_members_toggle_button(ui, theme, state, voice_viewport);
            }
        });

    // Screen source picker dialog (before starting stream).
    if state.main.show_screen_source_picker {
        poll_screen_source_preview_updates(ctx, state);
        let preview_refresh_needed = state.main.screen_source_preview_requested_tab
            != Some(state.main.screen_source_tab)
            || state
                .main
                .screen_source_preview_last_refresh
                .map(|last| last.elapsed() >= SCREEN_SOURCE_PREVIEW_REFRESH_INTERVAL)
                .unwrap_or(true);
        if preview_refresh_needed && !state.main.screen_source_preview_inflight {
            queue_screen_source_preview_refresh(ctx, state);
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let mut close_picker = false;
        let mut picked_source: Option<ScreenSourceEntry> = None;
        let mut tab_changed = false;
        let start_after_pick = state.main.start_stream_after_source_pick;
        let screen_sources = state.main.screen_sources.clone();
        let window_sources = state.main.window_sources.clone();
        let selected_source = state.main.selected_stream_source.clone();
        let preview_textures = state.main.screen_source_preview_textures.clone();
        let picker_overlay_rect = ctx.screen_rect();
        let picker_bottom_reserved = if server_selected {
            left_user_panel_height
        } else {
            0.0
        };
        let picker_outer_rect = inset_modal_rect(
            picker_overlay_rect,
            SCREEN_SOURCE_PICKER_HORIZONTAL_MARGIN,
            SCREEN_SOURCE_PICKER_VERTICAL_MARGIN,
            picker_bottom_reserved,
        );
        let picker_window_frame = egui::Frame::window(ctx.style().as_ref())
            .fill(theme.bg_tertiary)
            .stroke(egui::Stroke::new(1.0, theme.border_strong))
            .shadow(ctx.style().visuals.window_shadow);
        let picker_inner_size =
            window_inner_size_for_outer(ctx, picker_outer_rect.size(), &picker_window_frame);

        show_modal_backdrop(ctx, "screen_source_picker_backdrop", picker_overlay_rect);

        egui::Window::new(SCREEN_SOURCE_PICKER_TITLE)
            .collapsible(false)
            .resizable(false)
            .frame(picker_window_frame)
            .fixed_pos(picker_outer_rect.min)
            .fixed_size(picker_inner_size)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.heading(SCREEN_SOURCE_PICKER_TITLE);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            state.main.screen_source_tab == ScreenSourceTab::Applications,
                            SCREEN_SOURCE_TAB_APPLICATIONS,
                        )
                        .clicked()
                    {
                        state.main.screen_source_tab = ScreenSourceTab::Applications;
                        tab_changed = true;
                    }
                    if ui
                        .selectable_label(
                            state.main.screen_source_tab == ScreenSourceTab::EntireScreen,
                            SCREEN_SOURCE_TAB_ENTIRE_SCREEN,
                        )
                        .clicked()
                    {
                        state.main.screen_source_tab = ScreenSourceTab::EntireScreen;
                        tab_changed = true;
                    }
                });
                if tab_changed {
                    invalidate_screen_source_previews(state);
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                let footer_reserved_height = if start_after_pick { 44.0 } else { 84.0 };
                let grid_max_height = (ui.available_height() - footer_reserved_height).max(120.0);
                match state.main.screen_source_tab {
                    ScreenSourceTab::Applications => {
                        if window_sources.is_empty() {
                            ui.label(SCREEN_SOURCE_LABEL_NO_WINDOWS);
                        } else {
                            egui::ScrollArea::vertical()
                                .max_height(grid_max_height)
                                .show(ui, |ui| {
                                    let previews_loading = state.main.screen_source_preview_inflight
                                        && state.main.screen_source_preview_requested_tab
                                            == Some(ScreenSourceTab::Applications);
                                    if let Some(source) = show_screen_source_tile_grid(
                                        ui,
                                        theme,
                                        &window_sources,
                                        &preview_textures,
                                        &selected_source,
                                        previews_loading,
                                    ) {
                                        picked_source = Some(source);
                                        close_picker = true;
                                    }
                                });
                        }
                    }
                    ScreenSourceTab::EntireScreen => {
                        if screen_sources.is_empty() {
                            ui.label(SCREEN_SOURCE_LABEL_NO_SCREENS);
                        } else {
                            egui::ScrollArea::vertical()
                                .max_height(grid_max_height)
                                .show(ui, |ui| {
                                    let previews_loading = state.main.screen_source_preview_inflight
                                        && state.main.screen_source_preview_requested_tab
                                            == Some(ScreenSourceTab::EntireScreen);
                                    if let Some(source) = show_screen_source_tile_grid(
                                        ui,
                                        theme,
                                        &screen_sources,
                                        &preview_textures,
                                        &selected_source,
                                        previews_loading,
                                    ) {
                                        picked_source = Some(source);
                                        close_picker = true;
                                    }
                                });
                        }
                    }
                }
                ui.add_space(8.0);
                if !start_after_pick {
                    ui.label(SCREEN_SOURCE_PICKER_HELP_TEXT);
                    ui.add_space(8.0);
                }
                if ui.button(SCREEN_SOURCE_CANCEL_LABEL).clicked() {
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
            state.main.screen_source_preview_rx = None;
            state.main.screen_source_preview_inflight = false;
        } else if tab_changed {
            queue_screen_source_preview_refresh(ctx, state);
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
                }
                let controls_rect = egui::Rect::from_min_size(
                    screen.left_top() + egui::vec2(16.0, 16.0),
                    egui::vec2(220.0, 36.0),
                );
                ui.allocate_ui_at_rect(controls_rect, |ui| {
                    ui.horizontal(|ui| {
                        show_stream_audio_button(ui, state, engine_tx, uid);
                        if ui.button("✕ Отключиться").clicked()
                        {
                            set_stream_subscription(state, engine_tx, uid, false);
                            state.main.fullscreen_stream_user = None;
                        }
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

const VOICE_GRID_TILE_ASPECT: f32 = 16.0 / 9.0;
const VOICE_GRID_TILE_GAP: f32 = 14.0;
const VOICE_GRID_TILE_ROUNDING: f32 = 16.0;
const VOICE_GRID_TILE_PADDING: f32 = 12.0;
const VOICE_GRID_MEMBERS_BUTTON_SIZE: f32 = 30.0;
const VOICE_GRID_MEMBERS_BUTTON_MARGIN: f32 = 10.0;

#[derive(Clone, Copy)]
struct VoiceGridLayout {
    cols: usize,
    rows: usize,
    tile_size: egui::Vec2,
}

#[derive(Clone, Copy)]
enum VoiceMuteBadge {
    Full,
    Local,
    Mic,
}

fn show_voice_grid(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut State,
    api: &ApiClient,
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    participants: &[VoiceParticipant],
    speaking_snap: &HashMap<i64, bool>,
    in_this_voice: bool,
) {
    if let Some(focused_user) = state.main.voice_grid_focus_user {
        if !participants
            .iter()
            .any(|participant| participant.user_id == focused_user)
        {
            state.main.voice_grid_focus_user = None;
        }
    }

    let viewport = ui.available_rect_before_wrap();
    if viewport.width() <= 1.0 || viewport.height() <= 1.0 {
        return;
    }
    ui.allocate_rect(viewport, egui::Sense::hover());

    if let Some(focused_user) = state.main.voice_grid_focus_user {
        if let Some(focused_participant) = participants
            .iter()
            .find(|participant| participant.user_id == focused_user)
        {
            let others: Vec<&VoiceParticipant> = participants
                .iter()
                .filter(|participant| participant.user_id != focused_user)
                .collect();
            let strip_height = if others.is_empty() {
                0.0
            } else {
                (viewport.height() * 0.22).clamp(108.0, 160.0)
            };
            let main_area = if strip_height > 0.0 {
                egui::Rect::from_min_max(
                    viewport.min,
                    egui::pos2(
                        viewport.right(),
                        viewport.bottom() - strip_height - VOICE_GRID_TILE_GAP,
                    ),
                )
            } else {
                viewport
            };
            let main_size = fit_size_to_aspect(
                main_area.size() - egui::vec2(VOICE_GRID_TILE_PADDING * 2.0, 8.0),
                VOICE_GRID_TILE_ASPECT,
            );
            let main_rect = egui::Rect::from_center_size(main_area.center(), main_size);
            show_voice_participant_tile(
                ui,
                theme,
                state,
                api,
                engine_tx,
                main_rect,
                focused_participant,
                in_this_voice,
                *speaking_snap
                    .get(&focused_participant.user_id)
                    .unwrap_or(&false),
            );

            if !others.is_empty() {
                let strip_rect = egui::Rect::from_min_max(
                    egui::pos2(viewport.left(), viewport.bottom() - strip_height),
                    viewport.max,
                );
                let count = others.len();
                let thumb_max_width = (strip_rect.width()
                    - VOICE_GRID_TILE_GAP * count.saturating_sub(1) as f32)
                    / count as f32;
                let thumb_size = fit_size_to_aspect(
                    egui::vec2(thumb_max_width.max(56.0), strip_rect.height() - 6.0),
                    VOICE_GRID_TILE_ASPECT,
                );
                let row_width = thumb_size.x * count as f32
                    + VOICE_GRID_TILE_GAP * count.saturating_sub(1) as f32;
                let start_x = strip_rect.left() + ((strip_rect.width() - row_width).max(0.0) * 0.5);
                let start_y =
                    strip_rect.top() + ((strip_rect.height() - thumb_size.y).max(0.0) * 0.5);
                for (idx, participant) in others.iter().enumerate() {
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(
                            start_x + idx as f32 * (thumb_size.x + VOICE_GRID_TILE_GAP),
                            start_y,
                        ),
                        thumb_size,
                    );
                    show_voice_participant_tile(
                        ui,
                        theme,
                        state,
                        api,
                        engine_tx,
                        rect,
                        participant,
                        in_this_voice,
                        *speaking_snap.get(&participant.user_id).unwrap_or(&false),
                    );
                }
            }
            return;
        }
    }

    let layout = best_voice_grid_layout(participants.len(), viewport.size());
    let total_height = layout.tile_size.y * layout.rows as f32
        + VOICE_GRID_TILE_GAP * layout.rows.saturating_sub(1) as f32;
    let start_y = viewport.top() + ((viewport.height() - total_height).max(0.0) * 0.5);

    for row_idx in 0..layout.rows {
        let row_start = row_idx * layout.cols;
        let row_end = (row_start + layout.cols).min(participants.len());
        let row_participants = &participants[row_start..row_end];
        let row_width = layout.tile_size.x * row_participants.len() as f32
            + VOICE_GRID_TILE_GAP * row_participants.len().saturating_sub(1) as f32;
        let start_x = viewport.left() + ((viewport.width() - row_width).max(0.0) * 0.5);
        let y = start_y + row_idx as f32 * (layout.tile_size.y + VOICE_GRID_TILE_GAP);

        for (idx, participant) in row_participants.iter().enumerate() {
            let rect = egui::Rect::from_min_size(
                egui::pos2(
                    start_x + idx as f32 * (layout.tile_size.x + VOICE_GRID_TILE_GAP),
                    y,
                ),
                layout.tile_size,
            );
            show_voice_participant_tile(
                ui,
                theme,
                state,
                api,
                engine_tx,
                rect,
                participant,
                in_this_voice,
                *speaking_snap.get(&participant.user_id).unwrap_or(&false),
            );
        }
    }
}

fn show_voice_grid_members_toggle_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut State,
    viewport: egui::Rect,
) {
    if viewport.width() <= 1.0 || viewport.height() <= 1.0 {
        return;
    }

    let show_members = state.main.show_member_panel.unwrap_or(true);
    let tooltip = if show_members {
        "Скрыть участников"
    } else {
        "Показать панель участников"
    };
    let button_size = egui::vec2(
        VOICE_GRID_MEMBERS_BUTTON_SIZE,
        VOICE_GRID_MEMBERS_BUTTON_SIZE,
    );
    let button_rect = egui::Rect::from_min_size(
        egui::pos2(
            (viewport.right() - VOICE_GRID_MEMBERS_BUTTON_MARGIN - button_size.x)
                .max(viewport.left()),
            viewport.top() + VOICE_GRID_MEMBERS_BUTTON_MARGIN,
        ),
        button_size,
    );
    let fill = if show_members {
        Theme::lerp_color(theme.bg_quaternary, theme.bg_hover, 0.22)
    } else {
        Theme::lerp_color(theme.bg_quaternary, theme.accent, 0.18)
    };
    let text_color = if show_members {
        theme.text_secondary
    } else {
        theme.text_primary
    };
    let response = ui
        .put(
            button_rect,
            egui::Button::new(egui::RichText::new("M").size(12.0).color(text_color))
                .fill(fill)
                .stroke(egui::Stroke::new(1.0, theme.border))
                .rounding(egui::Rounding::same(8.0)),
        )
        .on_hover_text(tooltip);

    if response.clicked() {
        state.main.show_member_panel = Some(!show_members);
        ui.ctx().request_repaint();
    }
}

fn show_voice_participant_tile(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut State,
    api: &ApiClient,
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    rect: egui::Rect,
    participant: &VoiceParticipant,
    in_this_voice: bool,
    is_speaking: bool,
) {
    let id = ui.make_persistent_id(("voice_grid_tile", participant.user_id));
    let response = ui.interact(rect, id, egui::Sense::click());
    if response.clicked_by(egui::PointerButton::Primary) {
        if state.main.voice_grid_focus_user == Some(participant.user_id) {
            state.main.voice_grid_focus_user = None;
        } else {
            state.main.voice_grid_focus_user = Some(participant.user_id);
        }
        ui.ctx().request_repaint();
    }

    let is_focused = state.main.voice_grid_focus_user == Some(participant.user_id);
    let is_full_muted = participant.deafened;
    let is_locally_muted = in_this_voice
        && state
            .main
            .voice
            .locally_muted
            .contains(&participant.user_id);
    let can_open_context_menu = in_this_voice && Some(participant.user_id) != state.user_id;

    let hover_t = ui.ctx().animate_bool(id.with("hover"), response.hovered());
    let base_fill = if is_focused {
        Theme::lerp_color(theme.bg_active, theme.bg_hover, 0.55)
    } else {
        Theme::lerp_color(theme.bg_secondary, theme.bg_hover, hover_t * 0.5)
    };
    let border_color = if is_speaking {
        theme.success
    } else if is_focused {
        theme.accent
    } else {
        theme.border
    };
    let border_width = if is_speaking || is_focused { 2.0 } else { 1.0 };

    ui.painter().rect_filled(
        rect,
        egui::Rounding::same(VOICE_GRID_TILE_ROUNDING),
        base_fill,
    );
    ui.painter().rect_stroke(
        rect,
        egui::Rounding::same(VOICE_GRID_TILE_ROUNDING),
        egui::Stroke::new(border_width, border_color),
    );

    let content_rect = rect.shrink(VOICE_GRID_TILE_PADDING);
    let avatar_rect = content_rect;
    let footer_height = (avatar_rect.height() * 0.23).clamp(58.0, 92.0);
    let footer_rect = egui::Rect::from_min_max(
        egui::pos2(avatar_rect.left(), avatar_rect.bottom() - footer_height),
        avatar_rect.right_bottom(),
    );

    paint_voice_tile_media(
        ui,
        theme,
        state,
        engine_tx,
        participant,
        avatar_rect,
        in_this_voice,
        is_speaking,
    );

    ui.painter().rect_filled(
        footer_rect,
        egui::Rounding {
            nw: 0.0,
            ne: 0.0,
            sw: VOICE_GRID_TILE_ROUNDING,
            se: VOICE_GRID_TILE_ROUNDING,
        },
        egui::Color32::from_black_alpha(148),
    );

    let name = display_participant_name(participant);
    let name_font_size: f32 = if is_focused { 16.0 } else { 14.0 };
    let name_color = if is_locally_muted {
        theme.text_muted
    } else {
        theme.text_primary
    };
    let tag_height = if rect.height() < 170.0 { 20.0 } else { 22.0 };
    let mut right = footer_rect.right();
    if participant.streaming {
        let pill_rect = egui::Rect::from_min_max(
            egui::pos2(right - 46.0, footer_rect.top()),
            egui::pos2(right, footer_rect.top() + tag_height),
        );
        paint_tile_tag(
            ui.painter(),
            pill_rect,
            "LIVE",
            theme.error,
            theme.text_primary,
        );
        right = pill_rect.left() - 6.0;
    }
    if participant.cam_enabled {
        let pill_rect = egui::Rect::from_min_max(
            egui::pos2(right - 44.0, footer_rect.top()),
            egui::pos2(right, footer_rect.top() + tag_height),
        );
        paint_tile_tag(
            ui.painter(),
            pill_rect,
            "CAM",
            Theme::lerp_color(theme.bg_quaternary, theme.bg_hover, 0.45),
            theme.text_secondary,
        );
        right = pill_rect.left() - 6.0;
    }

    let name_rect = egui::Rect::from_min_max(
        egui::pos2(footer_rect.left(), footer_rect.top()),
        egui::pos2(
            right.max(footer_rect.left()),
            footer_rect.top() + tag_height,
        ),
    );
    let approx_char_width = (name_font_size * 0.58).max(6.0);
    let max_chars = (name_rect.width() / approx_char_width).floor().max(6.0) as usize;
    ui.painter().text(
        name_rect.left_center(),
        egui::Align2::LEFT_CENTER,
        truncate_with_ellipsis(name, max_chars),
        egui::FontId::proportional(name_font_size),
        name_color,
    );

    let badge_height = if rect.height() < 180.0 { 24.0 } else { 28.0 };
    let badge_top = (name_rect.bottom() + 8.0).min(footer_rect.bottom() - badge_height);
    let mut badge_x = footer_rect.left();
    if is_full_muted {
        let badge_rect = egui::Rect::from_min_size(
            egui::pos2(badge_x, badge_top),
            egui::vec2(58.0, badge_height),
        );
        paint_voice_mute_badge(ui.painter(), badge_rect, theme, VoiceMuteBadge::Full);
    } else {
        if is_locally_muted {
            let badge_rect = egui::Rect::from_min_size(
                egui::pos2(badge_x, badge_top),
                egui::vec2(28.0, badge_height),
            );
            paint_voice_mute_badge(ui.painter(), badge_rect, theme, VoiceMuteBadge::Local);
            badge_x = badge_rect.right() + 6.0;
        }
        if participant.mic_muted {
            let badge_rect = egui::Rect::from_min_size(
                egui::pos2(badge_x, badge_top),
                egui::vec2(28.0, badge_height),
            );
            paint_voice_mute_badge(ui.painter(), badge_rect, theme, VoiceMuteBadge::Mic);
        }
    }

    if can_open_context_menu {
        response.context_menu(|ui| {
            show_voice_participant_context_menu(
                ui,
                theme,
                state,
                api,
                engine_tx,
                participant.user_id,
            );
        });
    }
}

fn paint_voice_tile_media(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut State,
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    participant: &VoiceParticipant,
    avatar_rect: egui::Rect,
    in_this_voice: bool,
    is_speaking: bool,
) {
    let stream_key = video_frame_key(participant.user_id, true);
    let stream_preview_key = video_preview_frame_key(participant.user_id);
    let camera_key = participant.user_id;
    let has_stream_texture = state
        .main
        .voice_video_gpu_textures
        .contains_key(&stream_key)
        || state.main.voice_video_textures.contains_key(&stream_key);
    let has_stream_preview_texture = state
        .main
        .voice_video_gpu_textures
        .contains_key(&stream_preview_key)
        || state
            .main
            .voice_video_textures
            .contains_key(&stream_preview_key);
    let has_camera_texture = state
        .main
        .voice_video_gpu_textures
        .contains_key(&camera_key)
        || state.main.voice_video_textures.contains_key(&camera_key);
    let is_stream_subscribed = in_this_voice
        && state
            .main
            .voice
            .stream_subscriptions
            .contains(&participant.user_id);
    let show_stream_preview = participant.streaming && in_this_voice && !is_stream_subscribed;
    let show_stream_connecting =
        participant.streaming && in_this_voice && is_stream_subscribed && !has_stream_texture;
    let show_stream_controls =
        participant.streaming && in_this_voice && is_stream_subscribed && has_stream_texture;
    let tex_key = if participant.streaming {
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
        let rendered =
            if let Some(&(_, gl_tex_id, _, _)) = state.main.voice_video_gpu_textures.get(&key) {
                paint_wgl_video_texture(ui, avatar_rect, gl_tex_id);
                true
            } else {
                false
            };
        if !rendered {
            if let Some(texture) = state.main.voice_video_textures.get(&key) {
                ui.put(
                    avatar_rect,
                    egui::Image::new(texture).fit_to_exact_size(avatar_rect.size()),
                );
            }
        }

        if show_stream_preview {
            ui.painter().rect_filled(
                avatar_rect,
                egui::Rounding::same(VOICE_GRID_TILE_ROUNDING),
                egui::Color32::from_black_alpha(124),
            );
            let button_rect =
                egui::Rect::from_center_size(avatar_rect.center(), egui::vec2(126.0, 36.0));
            ui.allocate_ui_at_rect(button_rect, |ui| {
                if ui.button("Смотреть").clicked() {
                    set_stream_subscription(state, engine_tx, participant.user_id, true);
                }
            });
        }
        if show_stream_connecting {
            ui.painter().rect_filled(
                avatar_rect,
                egui::Rounding::same(VOICE_GRID_TILE_ROUNDING),
                egui::Color32::from_black_alpha(104),
            );
            ui.painter().text(
                avatar_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Подключение...",
                egui::FontId::proportional(16.0),
                egui::Color32::WHITE,
            );
        }
        if show_stream_controls {
            let button_size = egui::vec2(30.0, 30.0);
            let button_gap = 6.0;
            let right = avatar_rect.right() - 6.0;
            let top = avatar_rect.top() + 6.0;
            let disconnect_rect = egui::Rect::from_min_size(
                egui::pos2(right - button_size.x * 3.0 - button_gap * 2.0, top),
                button_size,
            );
            let fullscreen_rect = egui::Rect::from_min_size(
                egui::pos2(right - button_size.x * 2.0 - button_gap, top),
                button_size,
            );
            let audio_rect =
                egui::Rect::from_min_size(egui::pos2(right - button_size.x, top), button_size);
            ui.allocate_ui_at_rect(disconnect_rect, |ui| {
                if ui
                    .button("✕")
                    .on_hover_text("Отключиться от трансляции")
                    .clicked()
                {
                    set_stream_subscription(state, engine_tx, participant.user_id, false);
                }
            });
            ui.allocate_ui_at_rect(fullscreen_rect, |ui| {
                if ui
                    .button("⛶")
                    .on_hover_text("Развернуть трансляцию")
                    .clicked()
                {
                    state.main.fullscreen_stream_user = Some(participant.user_id);
                }
            });
            ui.allocate_ui_at_rect(audio_rect, |ui| {
                show_stream_audio_button(ui, state, engine_tx, participant.user_id);
            });

            let fps = state
                .main
                .voice_render_fps
                .get_mut(&key)
                .map(|tracker| tracker.update_and_get())
                .unwrap_or(0.0);
            if fps > 0.0 {
                let fps_rect = egui::Rect::from_min_size(
                    avatar_rect.left_bottom() + egui::vec2(8.0, -24.0),
                    egui::vec2(54.0, 18.0),
                );
                ui.painter().rect_filled(
                    fps_rect,
                    egui::Rounding::same(6.0),
                    egui::Color32::from_black_alpha(168),
                );
                ui.painter().text(
                    fps_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{fps:.0} fps"),
                    egui::FontId::proportional(11.0),
                    egui::Color32::WHITE,
                );
            }
        }
    } else {
        ui.painter().rect_filled(
            avatar_rect,
            egui::Rounding::same(VOICE_GRID_TILE_ROUNDING),
            egui::Color32::from_rgb(80, 100, 160),
        );
        let letter = participant
            .username
            .chars()
            .next()
            .map(|ch| ch.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());
        let font_size = (avatar_rect.height() * 0.38).max(18.0);
        ui.painter().text(
            avatar_rect.center(),
            egui::Align2::CENTER_CENTER,
            letter,
            egui::FontId::proportional(font_size),
            egui::Color32::WHITE,
        );
        if show_stream_preview {
            ui.painter().rect_filled(
                avatar_rect,
                egui::Rounding::same(VOICE_GRID_TILE_ROUNDING),
                egui::Color32::from_black_alpha(124),
            );
            let button_rect =
                egui::Rect::from_center_size(avatar_rect.center(), egui::vec2(126.0, 36.0));
            ui.allocate_ui_at_rect(button_rect, |ui| {
                if ui.button("Смотреть").clicked() {
                    set_stream_subscription(state, engine_tx, participant.user_id, true);
                }
            });
        }
        if show_stream_connecting {
            ui.painter().rect_filled(
                avatar_rect,
                egui::Rounding::same(VOICE_GRID_TILE_ROUNDING),
                egui::Color32::from_black_alpha(104),
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

    if is_speaking {
        ui.painter().rect_stroke(
            avatar_rect.expand(2.0),
            egui::Rounding::same(VOICE_GRID_TILE_ROUNDING + 2.0),
            egui::Stroke::new(2.0, theme.success),
        );
    }
}

fn show_voice_participant_context_menu(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut State,
    api: &ApiClient,
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: i64,
) {
    let is_locally_muted = state.main.voice.locally_muted.contains(&user_id);
    let mute_label = if is_locally_muted {
        "Снять локальный мут"
    } else {
        "Локально заглушить"
    };
    if ui.button(mute_label).clicked() {
        set_local_participant_muted(state, engine_tx, user_id, !is_locally_muted);
        ui.close_menu();
    }

    let denoise_enabled = state.main.voice.receiver_denoise_users.contains(&user_id);
    let denoise_label = if denoise_enabled {
        "Выключить шумоподавление (локально)"
    } else {
        "Включить шумоподавление (локально)"
    };
    if ui.button(denoise_label).clicked() {
        set_receiver_denoise_enabled(state, engine_tx, user_id, !denoise_enabled);
        ui.close_menu();
    }

    ui.label("Громкость 0-300%, по умолчанию 100%");
    let mut volume = state
        .main
        .voice
        .local_volumes
        .get(&user_id)
        .copied()
        .unwrap_or(1.0);
    if ui
        .add(
            egui::Slider::new(&mut volume, 0.0..=3.0)
                .step_by(0.01)
                .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                .text(""),
        )
        .changed()
    {
        state.main.voice.local_volumes.insert(user_id, volume);
        state
            .settings
            .voice_volume_by_user
            .insert(user_id.to_string(), volume);
        state.settings.save();
        sync_user_volume(engine_tx, &state.main.voice, user_id);
    }

    if can_manage_server_member(state, user_id) {
        ui.separator();
        if ui
            .add(
                egui::Button::new("Выгнать с сервера")
                    .fill(theme.error)
                    .stroke(egui::Stroke::NONE),
            )
            .clicked()
        {
            kick_server_member(ui.ctx(), state, api, user_id);
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
            ban_server_member(ui.ctx(), state, api, user_id);
            ui.close_menu();
        }
    }
}

fn display_participant_name(participant: &VoiceParticipant) -> &str {
    if participant.username.trim().is_empty() {
        "Гость"
    } else {
        participant.username.as_str()
    }
}

fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn best_voice_grid_layout(count: usize, available: egui::Vec2) -> VoiceGridLayout {
    let mut best = VoiceGridLayout {
        cols: 1,
        rows: count.max(1),
        tile_size: fit_size_to_aspect(available, VOICE_GRID_TILE_ASPECT),
    };
    let mut best_score = 0.0;

    for cols in 1..=count.max(1) {
        let rows = (count.max(1) + cols - 1) / cols;
        let max_width =
            (available.x - VOICE_GRID_TILE_GAP * cols.saturating_sub(1) as f32) / cols as f32;
        let max_height =
            (available.y - VOICE_GRID_TILE_GAP * rows.saturating_sub(1) as f32) / rows as f32;
        if max_width <= 0.0 || max_height <= 0.0 {
            continue;
        }
        let tile_size =
            fit_size_to_aspect(egui::vec2(max_width, max_height), VOICE_GRID_TILE_ASPECT);
        let balance_penalty = (cols as isize - rows as isize).abs() as f32 * 160.0;
        let empty_penalty = (cols * rows - count.max(1)) as f32 * 80.0;
        let score = tile_size.x * tile_size.y - balance_penalty - empty_penalty;
        if score > best_score {
            best_score = score;
            best = VoiceGridLayout {
                cols,
                rows,
                tile_size,
            };
        }
    }

    best
}

fn fit_size_to_aspect(max_size: egui::Vec2, aspect: f32) -> egui::Vec2 {
    let max_width = max_size.x.max(1.0);
    let max_height = max_size.y.max(1.0);
    let mut width = max_width;
    let mut height = width / aspect;
    if height > max_height {
        height = max_height;
        width = height * aspect;
    }
    egui::vec2(width.max(1.0), height.max(1.0))
}

fn paint_tile_tag(
    painter: &egui::Painter,
    rect: egui::Rect,
    label: &str,
    fill: egui::Color32,
    text_color: egui::Color32,
) {
    painter.rect_filled(rect, egui::Rounding::same(rect.height() * 0.45), fill);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        text_color,
    );
}

fn paint_voice_mute_badge(
    painter: &egui::Painter,
    rect: egui::Rect,
    theme: &Theme,
    badge: VoiceMuteBadge,
) {
    let (fill, icon_color) = match badge {
        VoiceMuteBadge::Full => (theme.error, theme.text_primary),
        VoiceMuteBadge::Local => (theme.warning, theme.bg_elevated),
        VoiceMuteBadge::Mic => (theme.error, theme.text_primary),
    };
    painter.rect_filled(rect, egui::Rounding::same(rect.height() * 0.5), fill);

    match badge {
        VoiceMuteBadge::Full => {
            let mic_rect = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 6.0, rect.top() + 4.0),
                egui::pos2(rect.center().x + 1.0, rect.bottom() - 4.0),
            );
            let headphones_rect = egui::Rect::from_min_max(
                egui::pos2(rect.center().x - 1.0, rect.top() + 4.0),
                egui::pos2(rect.right() - 6.0, rect.bottom() - 4.0),
            );
            paint_microphone_icon(painter, mic_rect, icon_color, true);
            paint_headphones_icon(painter, headphones_rect, icon_color, true);
        }
        VoiceMuteBadge::Local | VoiceMuteBadge::Mic => {
            let icon_rect = rect.shrink2(egui::vec2(4.0, 3.0));
            paint_microphone_icon(painter, icon_rect, icon_color, true);
        }
    }
}

fn paint_microphone_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    crossed: bool,
) {
    let stroke = egui::Stroke::new((rect.width() * 0.08).max(1.6), color);
    let body_w = rect.width() * 0.34;
    let body_h = rect.height() * 0.42;
    let body_rect = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + rect.height() * 0.34),
        egui::vec2(body_w, body_h),
    );
    painter.rect_stroke(body_rect, egui::Rounding::same(body_w * 0.5), stroke);
    painter.line_segment(
        [
            egui::pos2(body_rect.center().x, body_rect.bottom()),
            egui::pos2(body_rect.center().x, rect.bottom() - rect.height() * 0.24),
        ],
        stroke,
    );
    painter.add(egui::Shape::line(
        vec![
            egui::pos2(
                rect.center().x - rect.width() * 0.18,
                rect.bottom() - rect.height() * 0.28,
            ),
            egui::pos2(rect.center().x, rect.bottom() - rect.height() * 0.16),
            egui::pos2(
                rect.center().x + rect.width() * 0.18,
                rect.bottom() - rect.height() * 0.28,
            ),
        ],
        stroke,
    ));
    painter.line_segment(
        [
            egui::pos2(
                rect.center().x - rect.width() * 0.16,
                rect.bottom() - rect.height() * 0.08,
            ),
            egui::pos2(
                rect.center().x + rect.width() * 0.16,
                rect.bottom() - rect.height() * 0.08,
            ),
        ],
        stroke,
    );
    if crossed {
        painter.line_segment(
            [
                egui::pos2(
                    rect.left() + rect.width() * 0.18,
                    rect.bottom() - rect.height() * 0.1,
                ),
                egui::pos2(
                    rect.right() - rect.width() * 0.18,
                    rect.top() + rect.height() * 0.1,
                ),
            ],
            egui::Stroke::new(stroke.width + 0.3, color),
        );
    }
}

fn paint_headphones_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    crossed: bool,
) {
    let stroke = egui::Stroke::new((rect.width() * 0.08).max(1.6), color);
    let arc_radius = rect.width() * 0.28;
    let center = egui::pos2(rect.center().x, rect.top() + rect.height() * 0.46);
    let arc_points: Vec<egui::Pos2> = (0..=12)
        .map(|step| {
            let t = step as f32 / 12.0;
            let angle = egui::lerp(std::f32::consts::PI..=0.0, t);
            egui::pos2(
                center.x + angle.cos() * arc_radius,
                center.y - angle.sin() * arc_radius,
            )
        })
        .collect();
    painter.add(egui::Shape::line(arc_points, stroke));

    let cup_h = rect.height() * 0.24;
    let cup_w = rect.width() * 0.13;
    let left_cup = egui::Rect::from_center_size(
        egui::pos2(center.x - arc_radius, center.y + cup_h * 0.3),
        egui::vec2(cup_w, cup_h),
    );
    let right_cup = egui::Rect::from_center_size(
        egui::pos2(center.x + arc_radius, center.y + cup_h * 0.3),
        egui::vec2(cup_w, cup_h),
    );
    painter.rect_stroke(left_cup, egui::Rounding::same(cup_w * 0.4), stroke);
    painter.rect_stroke(right_cup, egui::Rounding::same(cup_w * 0.4), stroke);

    if crossed {
        painter.line_segment(
            [
                egui::pos2(
                    rect.left() + rect.width() * 0.18,
                    rect.bottom() - rect.height() * 0.1,
                ),
                egui::pos2(
                    rect.right() - rect.width() * 0.18,
                    rect.top() + rect.height() * 0.1,
                ),
            ],
            egui::Stroke::new(stroke.width + 0.3, color),
        );
    }
}

fn set_local_participant_muted(
    state: &mut State,
    engine_tx: &Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    user_id: i64,
    muted: bool,
) {
    if muted {
        state.main.voice.locally_muted.insert(user_id);
    } else {
        state.main.voice.locally_muted.remove(&user_id);
    }
    sync_user_volume(engine_tx, &state.main.voice, user_id);
}

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
                    .step_by(0.01)
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
