//! Точка входа eframe::App. Хранит состояние (legacy State из ui + Theme и AppState для новых панелей),
//! обрабатывает WS и голос, вызывает экраны из ui. После добавления панелей layout будет собираться здесь.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use eframe::egui;
use parking_lot::Mutex;
use std::sync::Arc;

use crate::crypto::ChannelKey;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::d3d11_gl_interop::{D3d11GlInterop, GL_INTEROP_AVAILABLE};
use crate::net::{
    new_event_queue, ws_task, ApiClient, AttachmentMeta, Channel, LoginRequest, Member, Message,
    RegisterRequest, Server, VoiceParticipant, WsClientMsg, WsEventQueue,
};
use crate::state::AppState;
use crate::theme::Theme;
use crate::ui::{
    self, auth_screen, block_on, find_attachment_mime, main_screen, process_background_loads, State,
};
use crate::voice::{video_frame_key, video_preview_frame_key, VideoFrames, VoiceCmd};

// ─── App ───────────────────────────────────────────────────────────────────

pub struct AstrixApp {
    /// Тема для новых панелей (Discord-like).
    pub theme: Theme,
    /// Состояние для нового UI (моки / будущие панели).
    pub app_state: AppState,
    state: Arc<Mutex<State>>,
    api: ApiClient,
    ws_events: WsEventQueue,
    ws_tx: Option<tokio::sync::mpsc::UnboundedSender<WsClientMsg>>,
    egui_ctx: Option<egui::Context>,
    media_textures: HashMap<i64, egui::TextureHandle>,
    avatar_textures: HashMap<i64, egui::TextureHandle>,
    avatar_pending: VecDeque<i64>,
    avatar_failed: HashSet<i64>,
    media_pending: VecDeque<i64>,
    media_bytes: HashMap<i64, (Vec<u8>, String)>,
    voice_engine_tx: Option<tokio::sync::mpsc::UnboundedSender<VoiceCmd>>,
    voice_engine_done: Option<std::sync::mpsc::Receiver<()>>,
    voice_video_frames: Option<VideoFrames>,
    /// Phase 3.5: WGL_NV_DX_interop2 manager for zero-copy D3D11→GL texture sharing.
    /// None until first update() (requires active GL context), or if init failed.
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    gl_interop: Option<D3d11GlInterop>,
    /// Prevents retrying WGL init on every frame after initial failure.
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    gl_interop_tried: bool,
}

impl AstrixApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let settings = ui::Settings::load();
        let api = ApiClient::new(Some(settings.api_base.clone()));
        let mut auth_state = ui::AuthState::default();
        if settings.remember_me {
            auth_state.username = settings.saved_username.clone();
            auth_state.password = settings.saved_password.clone();
            auth_state.remember_me = true;
        }
        let mut state = State {
            screen: ui::Screen::Auth,
            auth: auth_state,
            settings: settings.clone(),
            dark_mode: true,
            ..Default::default()
        };
        state.main.voice.input_sensitivity = settings.input_sensitivity;
        Self {
            theme: Theme::default(),
            app_state: AppState::default(),
            state: Arc::new(Mutex::new(state)),
            api,
            ws_events: new_event_queue(),
            ws_tx: None,
            egui_ctx: None,
            media_textures: HashMap::new(),
            avatar_textures: HashMap::new(),
            avatar_pending: VecDeque::new(),
            avatar_failed: HashSet::new(),
            media_pending: VecDeque::new(),
            media_bytes: HashMap::new(),
            voice_engine_tx: None,
            voice_engine_done: None,
            voice_video_frames: None,
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            gl_interop: None,
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            gl_interop_tried: false,
        }
    }

    fn connect_ws(&mut self, token: &str, server_id: i64, channel_id: Option<i64>) {
        self.ws_tx = None;
        let ws_base = self.api.ws_base();
        let mut ws_url = format!("{}/ws?token={}&server_id={}", ws_base, token, server_id);
        if let Some(ch) = channel_id {
            ws_url.push_str(&format!("&channel_id={}", ch));
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsClientMsg>();
        self.ws_tx = Some(tx);
        let events = self.ws_events.clone();
        let ctx = self.egui_ctx.clone().expect("egui_ctx must be set");
        tokio::spawn(ws_task(ws_url, events, rx, ctx));
    }

    fn ws_send(&self, msg: WsClientMsg) {
        if let Some(tx) = &self.ws_tx {
            let _ = tx.send(msg);
        }
    }

    fn ws_view_channel(&self, channel_id: i64, last_message_id: Option<i64>) {
        let payload = last_message_id
            .filter(|&id| id > 0)
            .map(|id| serde_json::json!({ "last_message_id": id }));
        self.ws_send(WsClientMsg {
            kind: "channel.view".into(),
            channel_id: Some(channel_id),
            payload,
        });
    }

    fn ws_typing(&self, channel_id: i64) {
        self.ws_send(WsClientMsg {
            kind: "typing".into(),
            channel_id: Some(channel_id),
            payload: None,
        });
    }

    fn process_ws_events(&mut self, ctx: &egui::Context) {
        let events: Vec<_> = {
            let mut q = self.ws_events.lock();
            q.drain(..).collect()
        };
        for ev in events {
            let mut st = self.state.lock();
            match ev.kind.as_str() {
                "message.created" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(msg) = serde_json::from_value::<Message>(payload.clone()) {
                            if Some(msg.channel_id) != st.main.selected_channel
                                && Some(msg.author_id) != st.user_id
                            {
                                st.main.unread_channels.insert(msg.channel_id);
                            }
                            if !st.main.messages.iter().any(|m| m.id == msg.id) {
                                st.main.messages.push(msg.clone());
                            } else {
                                for m in &mut st.main.messages {
                                    if m.id == msg.id {
                                        m.seen_by = msg.seen_by.clone();
                                        break;
                                    }
                                }
                            }
                            for att in &msg.attachments {
                                let mid = att.media_id;
                                if !self.media_textures.contains_key(&mid)
                                    && !self.media_bytes.contains_key(&mid)
                                {
                                    self.media_pending.push_back(mid);
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "message.updated" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(msg) = serde_json::from_value::<Message>(payload.clone()) {
                            if st.main.messages_load_for == Some(msg.channel_id) {
                                for m in &mut st.main.messages {
                                    if m.id == msg.id {
                                        m.content = msg.content.clone();
                                        m.attachments = msg.attachments.clone();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "message.deleted" => {
                    if let Some(payload) = &ev.payload {
                        let message_id = payload
                            .get("id")
                            .or_else(|| payload.get("message_id"))
                            .and_then(|v| v.as_i64());
                        if let Some(mid) = message_id {
                            st.main.messages.retain(|m| m.id != mid);
                        }
                    }
                    ctx.request_repaint();
                }
                "channel.created" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(ch) = serde_json::from_value::<Channel>(payload.clone()) {
                            if Some(ch.server_id) == st.main.selected_server {
                                if !st.main.channels.iter().any(|c| c.id == ch.id) {
                                    st.main.channels.push(ch);
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "channel.renamed" | "channel.updated" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(ch) = serde_json::from_value::<Channel>(payload.clone()) {
                            for c in &mut st.main.channels {
                                if c.id == ch.id {
                                    c.name = ch.name.clone();
                                    c.r#type = ch.r#type.clone();
                                    break;
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "channel.deleted" => {
                    if let Some(payload) = &ev.payload {
                        let channel_id = payload
                            .get("id")
                            .or_else(|| payload.get("channel_id"))
                            .and_then(|v| v.as_i64());
                        if let Some(cid) = channel_id {
                            let server_id = payload.get("server_id").and_then(|v| v.as_i64());
                            if server_id == st.main.selected_server
                                || ev.server_id == st.main.selected_server.unwrap_or(0)
                            {
                                st.main.channels.retain(|c| c.id != cid);
                                if st.main.selected_channel == Some(cid) {
                                    let fallback = st
                                        .main
                                        .channels
                                        .iter()
                                        .find(|c| c.r#type == "text")
                                        .map(|c| c.id);
                                    st.main.selected_channel = fallback;
                                    st.main.messages.clear();
                                    st.main.messages_load_for = None;
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "member.joined" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(m) = serde_json::from_value::<Member>(payload.clone()) {
                            if Some(ev.server_id) == st.main.selected_server {
                                if !st
                                    .main
                                    .server_members
                                    .iter()
                                    .any(|x| x.user_id == m.user_id)
                                {
                                    st.main.server_members.push(m);
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "member.left" => {
                    if let Some(payload) = &ev.payload {
                        let user_id = payload.get("user_id").and_then(|v| v.as_i64());
                        if let Some(uid) = user_id {
                            if Some(ev.server_id) == st.main.selected_server {
                                st.main.server_members.retain(|m| m.user_id != uid);
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "member.updated" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(m) = serde_json::from_value::<Member>(payload.clone()) {
                            if Some(ev.server_id) == st.main.selected_server {
                                if let Some(existing) = st
                                    .main
                                    .server_members
                                    .iter_mut()
                                    .find(|x| x.user_id == m.user_id)
                                {
                                    existing.username = m.username.clone();
                                    existing.display_name = m.display_name.clone();
                                    existing.is_owner = m.is_owner;
                                } else {
                                    st.main.server_members.push(m);
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "member.renamed" => {
                    if let Some(payload) = &ev.payload {
                        let user_id = payload.get("user_id").and_then(|v| v.as_i64());
                        let display = payload
                            .get("display_name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if let (Some(uid), Some(name)) = (user_id, display) {
                            for m in &mut st.main.server_members {
                                if m.user_id == uid {
                                    m.display_name = name.clone();
                                    break;
                                }
                            }
                            if Some(uid) == st.user_id
                                && Some(ev.server_id) == st.main.selected_server
                            {
                                st.main.my_display_name = name;
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "presence.init" => {
                    if let Some(payload) = &ev.payload {
                        if let Some(arr) = payload.get("online_user_ids").and_then(|v| v.as_array())
                        {
                            let ids: HashSet<i64> = arr.iter().filter_map(|v| v.as_i64()).collect();
                            st.main.online_users = ids;
                        }
                    }
                    ctx.request_repaint();
                }
                "presence.update" => {
                    if let Some(payload) = &ev.payload {
                        let user_id = payload.get("user_id").and_then(|v| v.as_i64());
                        let online = payload.get("online").and_then(|v| v.as_bool());
                        if let (Some(uid), Some(is_online)) = (user_id, online) {
                            if is_online {
                                st.main.online_users.insert(uid);
                            } else {
                                st.main.online_users.remove(&uid);
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "typing" => {
                    if let Some(payload) = &ev.payload {
                        let user_id = payload.get("user_id").and_then(|v| v.as_i64());
                        let username = payload
                            .get("username")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if let (Some(uid), Some(name)) = (user_id, username) {
                            if Some(ev.channel_id) == st.main.selected_channel {
                                let entry = st
                                    .main
                                    .typing_users
                                    .iter_mut()
                                    .find(|(id, _, _)| *id == uid);
                                if let Some(e) = entry {
                                    e.2 = Instant::now();
                                } else {
                                    st.main.typing_users.push((uid, name, Instant::now()));
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "voice.participant_joined" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(p) = serde_json::from_value::<VoiceParticipant>(payload.clone()) {
                            let vol = st
                                .settings
                                .voice_volume_by_user
                                .get(&p.user_id.to_string())
                                .copied()
                                .unwrap_or(1.0);
                            let stream_vol = st
                                .settings
                                .stream_volume_by_user
                                .get(&p.user_id.to_string())
                                .copied()
                                .unwrap_or(1.0);
                            let denoise_enabled = st
                                .settings
                                .receiver_denoise_by_user
                                .contains(&p.user_id.to_string());
                            st.main.voice.local_volumes.insert(p.user_id, vol);
                            st.main.voice.stream_volumes.insert(p.user_id, stream_vol);
                            if denoise_enabled {
                                st.main.voice.receiver_denoise_users.insert(p.user_id);
                            } else {
                                st.main.voice.receiver_denoise_users.remove(&p.user_id);
                            }
                            if let Some(tx) = self.voice_engine_tx.as_ref() {
                                tx.send(VoiceCmd::SetUserVolume(p.user_id, vol)).ok();
                                tx.send(VoiceCmd::SetStreamVolume(p.user_id, stream_vol))
                                    .ok();
                                tx.send(VoiceCmd::SetRemoteVoiceDenoise {
                                    user_id: p.user_id,
                                    enabled: denoise_enabled,
                                })
                                .ok();
                            }
                            if let Some(ch_id) = p.channel_id {
                                let list = st.main.channel_voice.entry(ch_id).or_default();
                                if !list.iter().any(|x| x.user_id == p.user_id) {
                                    list.push(p.clone());
                                }
                            }
                            if st.main.voice.channel_id == p.channel_id {
                                if !st
                                    .main
                                    .voice
                                    .participants
                                    .iter()
                                    .any(|x| x.user_id == p.user_id)
                                {
                                    st.main.voice.participants.push(p);
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "voice.participant_left" => {
                    if let Some(payload) = &ev.payload {
                        let user_id = payload.get("user_id").and_then(|v| v.as_i64());
                        let channel_id = payload.get("channel_id").and_then(|v| v.as_i64());
                        if let Some(uid) = user_id {
                            if let Some(ch_id) = channel_id {
                                if let Some(list) = st.main.channel_voice.get_mut(&ch_id) {
                                    list.retain(|p| p.user_id != uid);
                                }
                            }
                            st.main.voice.participants.retain(|p| p.user_id != uid);
                            st.main.voice.locally_muted.remove(&uid);
                            st.main.voice.stream_muted.remove(&uid);
                            st.main.voice.stream_subscriptions.remove(&uid);
                            st.main
                                .voice_video_textures
                                .remove(&video_frame_key(uid, true));
                            st.main
                                .voice_video_textures
                                .remove(&video_preview_frame_key(uid));
                            if let Some((egui_tex_id, _, _, _)) = st
                                .main
                                .voice_video_gpu_textures
                                .remove(&video_frame_key(uid, true))
                            {
                                st.main.voice_video_gpu_tex_pending_delete.push(egui_tex_id);
                            }
                            if let Some((egui_tex_id, _, _, _)) = st
                                .main
                                .voice_video_gpu_textures
                                .remove(&video_preview_frame_key(uid))
                            {
                                st.main.voice_video_gpu_tex_pending_delete.push(egui_tex_id);
                            }
                            st.main.voice_render_fps.remove(&video_frame_key(uid, true));
                            if st.main.fullscreen_stream_user == Some(uid) {
                                st.main.fullscreen_stream_user = None;
                            }
                            if Some(uid) == st.user_id {
                                if st.main.voice.channel_id
                                    == channel_id.or(st.main.voice.channel_id)
                                {
                                    st.main.voice.channel_id = None;
                                    st.main.voice.server_id = None;
                                    st.main.voice.participants.clear();
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "voice.state_update" => {
                    if let Some(payload) = &ev.payload {
                        let user_id = payload.get("user_id").and_then(|v| v.as_i64());
                        let channel_id = payload.get("channel_id").and_then(|v| v.as_i64());
                        if let Some(uid) = user_id {
                            let mut streaming_now = None;
                            let apply = |p: &mut VoiceParticipant| {
                                if let Some(v) = payload.get("mic_muted").and_then(|v| v.as_bool())
                                {
                                    p.mic_muted = v;
                                }
                                if let Some(v) =
                                    payload.get("cam_enabled").and_then(|v| v.as_bool())
                                {
                                    p.cam_enabled = v;
                                }
                                if let Some(v) = payload.get("streaming").and_then(|v| v.as_bool())
                                {
                                    p.streaming = v;
                                }
                            };
                            for p in &mut st.main.voice.participants {
                                if p.user_id == uid {
                                    streaming_now = payload
                                        .get("streaming")
                                        .and_then(|v| v.as_bool())
                                        .or(Some(p.streaming));
                                    apply(p);
                                    break;
                                }
                            }
                            if let Some(ch_id) = channel_id {
                                if let Some(list) = st.main.channel_voice.get_mut(&ch_id) {
                                    for p in list.iter_mut() {
                                        if p.user_id == uid {
                                            if streaming_now.is_none() {
                                                streaming_now = payload
                                                    .get("streaming")
                                                    .and_then(|v| v.as_bool())
                                                    .or(Some(p.streaming));
                                            }
                                            apply(p);
                                            break;
                                        }
                                    }
                                }
                            }
                            if matches!(streaming_now, Some(false)) {
                                st.main.voice.stream_muted.remove(&uid);
                                st.main.voice.stream_subscriptions.remove(&uid);
                                if st.main.fullscreen_stream_user == Some(uid) {
                                    st.main.fullscreen_stream_user = None;
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "server.deleted" => {
                    if let Some(payload) = &ev.payload {
                        let server_id = payload.get("server_id").and_then(|v| v.as_i64());
                        if let Some(sid) = server_id {
                            st.main.servers.retain(|s| s.id != sid);
                            if st.main.selected_server == Some(sid) {
                                st.main.selected_server = None;
                                st.main.channels.clear();
                                st.main.server_members.clear();
                                st.main.selected_channel = None;
                                st.main.messages.clear();
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "server.updated" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(srv) = serde_json::from_value::<Server>(payload.clone()) {
                            for s in &mut st.main.servers {
                                if s.id == srv.id {
                                    s.name = srv.name.clone();
                                    s.owner_id = srv.owner_id;
                                    break;
                                }
                            }
                        } else {
                            let server_id = payload
                                .get("server_id")
                                .or_else(|| payload.get("id"))
                                .and_then(|v| v.as_i64());
                            let name = payload
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            if let (Some(sid), Some(name)) = (server_id, name) {
                                for s in &mut st.main.servers {
                                    if s.id == sid {
                                        s.name = name;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "server.added" => {
                    if let Some(payload) = &ev.payload {
                        if let Ok(srv) = serde_json::from_value::<Server>(payload.clone()) {
                            if !st.main.servers.iter().any(|s| s.id == srv.id) {
                                st.main.servers.push(srv);
                            }
                        } else {
                            let server_id = payload
                                .get("server_id")
                                .or_else(|| payload.get("id"))
                                .and_then(|v| v.as_i64());
                            let name = payload
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let owner_id = payload
                                .get("owner_id")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            if let (Some(sid), Some(name)) = (server_id, name) {
                                if !st.main.servers.iter().any(|s| s.id == sid) {
                                    st.main.servers.push(Server {
                                        id: sid,
                                        name,
                                        owner_id,
                                    });
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                "user.updated" => {
                    let uid_opt = ev.payload.as_ref().and_then(|p| {
                        p.get("user_id")
                            .or_else(|| p.get("id"))
                            .and_then(|v| v.as_i64())
                    });
                    let username_opt = ev.payload.as_ref().and_then(|p| {
                        p.get("username")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    });
                    // Only invalidate avatar cache when avatar_changed is explicitly true.
                    // Default to false to avoid constant re-fetches on other user.updated events (e.g. nickname).
                    let avatar_changed = ev
                        .payload
                        .as_ref()
                        .and_then(|p| p.get("avatar_changed").and_then(|v| v.as_bool()))
                        .unwrap_or(false);
                    if let Some(uid) = uid_opt {
                        if let Some(ref username) = username_opt {
                            for m in &mut st.main.server_members {
                                if m.user_id == uid {
                                    m.username = username.clone();
                                    break;
                                }
                            }
                            for msg in &mut st.main.messages {
                                if msg.author_id == uid {
                                    msg.author_username = username.clone();
                                }
                            }
                        }
                        if avatar_changed {
                            drop(st);
                            self.avatar_textures.remove(&uid);
                            self.avatar_failed.remove(&uid);
                            self.avatar_pending.retain(|&x| x != uid);
                        }
                    }
                    ctx.request_repaint();
                }
                "messages.read" => {
                    if let Some(payload) = &ev.payload {
                        let reader_id = payload.get("reader_id").and_then(|v| v.as_i64());
                        let channel_id = payload.get("channel_id").and_then(|v| v.as_i64());
                        if let (Some(rid), Some(cid)) = (reader_id, channel_id) {
                            if Some(rid) == st.user_id {
                                st.main.unread_channels.remove(&cid);
                            }
                            if st.main.messages_load_for == Some(cid) {
                                for msg in &mut st.main.messages {
                                    if msg.author_id != rid && !msg.seen_by.contains(&rid) {
                                        msg.seen_by.push(rid);
                                    }
                                }
                            }
                        }
                    }
                    ctx.request_repaint();
                }
                _ => {}
            }
        }
    }

    fn queue_avatar_pending(&mut self, state: &State) {
        for m in &state.main.server_members {
            let uid = m.user_id;
            if !self.avatar_textures.contains_key(&uid)
                && !self.avatar_failed.contains(&uid)
                && !self.avatar_pending.iter().any(|&x| x == uid)
            {
                self.avatar_pending.push_back(uid);
            }
        }
    }

    fn process_avatar_downloads(&mut self, _state: &mut State) {
        if let Some(uid) = self.avatar_pending.pop_front() {
            if self.avatar_textures.contains_key(&uid) || self.avatar_failed.contains(&uid) {
                return;
            }
            let api = self.api.clone();
            match block_on(api.get_avatar(uid)) {
                Ok(bytes) if !bytes.is_empty() => {
                    if let Ok(img) = image::load_from_memory(&bytes) {
                        let rgba = img.to_rgba8();
                        let size = [img.width() as usize, img.height() as usize];
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                        if let Some(ctx) = &self.egui_ctx {
                            let handle = ctx.load_texture(
                                format!("avatar_{}", uid),
                                color_image,
                                egui::TextureOptions::LINEAR,
                            );
                            self.avatar_textures.insert(uid, handle);
                        } else {
                            self.avatar_failed.insert(uid);
                        }
                    } else {
                        // Decode failed (corrupt/unsupported format) — stop retrying
                        self.avatar_failed.insert(uid);
                    }
                }
                Err(_) => {
                    self.avatar_failed.insert(uid);
                }
                _ => {
                    // Empty response — no avatar, don't retry
                    self.avatar_failed.insert(uid);
                }
            }
        }
    }

    fn process_media_downloads(&mut self, state: &mut State) {
        if let Some(media_id) = self.media_pending.pop_front() {
            if self.media_bytes.contains_key(&media_id)
                || self.media_textures.contains_key(&media_id)
            {
                return;
            }
            if let Some(token) = state.access_token.clone() {
                let api = self.api.clone();
                if let Ok(bytes) = block_on(api.download_media(&token, media_id)) {
                    if let Ok(img) = image::load_from_memory(&bytes) {
                        let rgba = img.to_rgba8();
                        let size = [img.width() as usize, img.height() as usize];
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                        if let Some(ctx) = &self.egui_ctx {
                            let handle = ctx.load_texture(
                                format!("media_{}", media_id),
                                color_image,
                                egui::TextureOptions::LINEAR,
                            );
                            self.media_textures.insert(media_id, handle);
                        }
                    }
                    let mime = find_attachment_mime(&state.main.messages, media_id);
                    self.media_bytes.insert(media_id, (bytes, mime));
                }
            }
        }
    }
}

impl eframe::App for AstrixApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.egui_ctx.is_none() {
            self.egui_ctx = Some(ctx.clone());
        }

        // Phase 3.5: Release WGL interop locks from the previous frame so D3D11 can write.
        // Must run at the START of update(), before we re-lock for the current frame.
        #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
        if let Some(interop) = &mut self.gl_interop {
            interop.unlock_all();
        }

        // Phase 3.5: Initialize WGL_NV_DX_interop2 once the MFT shared device is ready.
        // IMPORTANT: must use the same D3D11 device as D3d11Nv12ToRgba (compute shader).
        // wglDXLockObjectsNV only flushes/waits for the device it was opened with — using
        // a different device causes a black screen (GL reads before compute finishes writing).
        //
        // We retry every frame until get_shared_device() returns Some (i.e., until a voice
        // session initialises the MFT device). gl_interop_tried is only set to true once we
        // have the device, so early frames before voice don't permanently disable interop.
        #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
        if !self.gl_interop_tried && frame.gl().is_some() {
            if let Some(shared_dev) = crate::mft_device::get_shared_device() {
                // Mark tried only once we have the device — prevents retry on real failures.
                self.gl_interop_tried = true;
                match D3d11GlInterop::try_new(shared_dev) {
                    Ok(interop) => {
                        GL_INTEROP_AVAILABLE.store(true, std::sync::atomic::Ordering::Release);
                        let zero_copy_enabled = crate::voice_livekit::wgl_zero_copy_enabled();
                        eprintln!(
                            "[Phase 3.5] WGL_NV_DX_interop2 ready — GPU zero-copy path {}",
                            if zero_copy_enabled {
                                "enabled by default"
                            } else {
                                "disabled via ASTRIX_VIDEO_DISABLE_WGL_INTEROP=1"
                            }
                        );
                        self.gl_interop = Some(interop);
                    }
                    Err(e) => {
                        eprintln!(
                            "[Phase 3.5] WGL_NV_DX_interop2 unavailable: {e} — using CPU readback"
                        );
                    }
                }
            }
            // If get_shared_device() == None, skip silently and retry next frame.
        }
        // Obtain Arc<glow::Context> for GL texture management (passed to main_screen).
        #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
        let gl_ctx = frame.gl().cloned();

        self.process_ws_events(ctx);
        ctx.request_repaint_after(Duration::from_millis(500));

        let mut guard = self.state.lock();

        guard.dark_mode = true;
        self.theme.apply_egui_visuals(ctx);
        guard.settings.dark_mode = true;

        if guard.main.selected_server != guard.main.ws_connected_server
            && guard.main.selected_server.is_some()
            && guard.access_token.is_some()
        {
            let token = guard.access_token.clone().unwrap();
            let server_id = guard.main.selected_server.unwrap();
            let channel_id = guard.main.selected_channel;
            guard.main.ws_connected_server = Some(server_id);
            guard.main.online_users.clear();
            drop(guard);
            self.connect_ws(&token, server_id, channel_id);
            guard = self.state.lock();
        }

        {
            drop(guard);
            let state_arc = Arc::clone(&self.state);
            {
                let st = state_arc.lock();
                self.queue_avatar_pending(&st);
            }
            {
                let mut st = state_arc.lock();
                self.process_avatar_downloads(&mut st);
                self.process_media_downloads(&mut st);
            }
            guard = self.state.lock();
        }

        match guard.screen {
            ui::Screen::Auth => {
                drop(guard);
                let mut st = self.state.lock();
                auth_screen(ctx, &mut st, &self.api);
                drop(st);
                return;
            }
            ui::Screen::Main => {}
        }
        drop(guard);

        // Неблокирующая загрузка: запуск фоновых задач при смене сервера/канала.
        process_background_loads(ctx.clone(), Arc::clone(&self.state), self.api.clone());

        let mut st = self.state.lock();

        if st.main.selected_channel != st.main.ws_viewing_channel {
            let ch = st.main.selected_channel;
            st.main.ws_viewing_channel = ch;
            if let Some(cid) = ch {
                drop(st);
                self.ws_view_channel(cid, None);
                st = self.state.lock();
            }
        }

        let now = Instant::now();
        st.main
            .typing_users
            .retain(|(_, _, t)| now.duration_since(*t) < Duration::from_secs(3));

        if !st.main.new_message.is_empty() {
            let changed = st.main.new_message != st.main.prev_message;
            if changed {
                let should_send = match st.main.last_typing_sent {
                    Some(t) => now.duration_since(t) > Duration::from_secs(1),
                    None => true,
                };
                if should_send {
                    if let Some(ch_id) = st.main.selected_channel {
                        st.main.last_typing_sent = Some(now);
                        drop(st);
                        self.ws_typing(ch_id);
                        st = self.state.lock();
                    }
                }
            }
        }
        st.main.prev_message = st.main.new_message.clone();

        let should_logout = main_screen(
            ctx,
            &mut st,
            &self.theme,
            &self.api,
            &self.media_textures,
            &self.media_bytes,
            &self.avatar_textures,
            &mut self.voice_engine_tx,
            &mut self.voice_engine_done,
            &mut self.voice_video_frames,
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            gl_ctx,
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            self.gl_interop.as_mut(),
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            frame,
        );

        // Phase 3.5: Lock all WGL interop objects for the upcoming GL render pass.
        // Must be called after building egui UI, before eframe renders to screen.
        // Matching unlock_all() is called in post_rendering().
        #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
        if let Some(interop) = &mut self.gl_interop {
            interop.lock_all();
        }

        if let Some((cid, mid)) = st.main.pending_read_receipt.take() {
            drop(st);
            self.ws_view_channel(cid, Some(mid));
            st = self.state.lock();
        }
        if !st.main.pending_media_ids.is_empty() {
            let ids: Vec<i64> = st.main.pending_media_ids.drain(..).collect();
            drop(st);
            for id in ids {
                if !self.media_textures.contains_key(&id) && !self.media_bytes.contains_key(&id) {
                    self.media_pending.push_back(id);
                }
            }
            st = self.state.lock();
        }

        if should_logout {
            if !st.settings.remember_me {
                st.settings.saved_username.clear();
                st.settings.saved_password.clear();
            }
            st.settings.save();
            self.ws_tx = None;
            st.access_token = None;
            st.user_id = None;
            st.main = ui::MainState::default();
            st.main.voice.input_sensitivity = st.settings.input_sensitivity;
            st.screen = ui::Screen::Auth;
            ctx.request_repaint();
        }
    }
}
