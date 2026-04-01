use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_API_BASE: &str = "http://193.233.251.173:8080";

// ─── Auth ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub user_id: i64,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub public_e2ee_key: Option<Vec<u8>>,
}

// ─── Domain types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub owner_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub server_id: i64,
    pub name: String,
    pub r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub user_id: i64,
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub is_owner: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePreview {
    pub token: String,
    pub server_id: i64,
    pub server_name: String,
    #[serde(default)]
    pub owner_id: i64,
    #[serde(default)]
    pub channel_id: Option<i64>,
    #[serde(default)]
    pub channel_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteAcceptResponse {
    pub server: Server,
    #[serde(default)]
    pub channel_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AttachmentMeta {
    pub media_id: i64,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub channel_id: i64,
    pub author_id: i64,
    pub author_username: String,
    pub content: String,
    pub created_at: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentMeta>,
    #[serde(default)]
    pub seen_by: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaUploadResponse {
    pub id: i64,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
}

// ─── Voice types ──────────────────────────────────────────────────────────────

/// A single ICE server entry returned by /voice/join.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

/// A voice channel participant (from server presence state).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceParticipant {
    pub user_id: i64,
    pub username: String,
    #[serde(default)]
    pub mic_muted: bool,
    #[serde(default)]
    pub deafened: bool,
    #[serde(default)]
    pub cam_enabled: bool,
    #[serde(default)]
    pub streaming: bool,
    /// Present in WS broadcast events (voice.participant_joined) so the
    /// client can maintain per-channel lists. Absent in REST responses.
    #[serde(default)]
    pub channel_id: Option<i64>,
}

/// Response from POST /voice/join (LiveKit: url + token; participants from server).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceJoinResponse {
    /// LiveKit WebSocket URL (e.g. ws://localhost:7880).
    pub livekit_url: String,
    /// LiveKit JWT access token.
    pub token: String,
    pub participants: Vec<VoiceParticipant>,
    /// Legacy field; server no longer sends this after Phase 1.
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
}

// ─── WebSocket event types ────────────────────────────────────────────────────

/// An event received from the server via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsServerEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub server_id: i64,
    #[serde(default)]
    pub channel_id: i64,
    pub payload: Option<serde_json::Value>,
}

/// A message sent from the client to the server via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsClientMsg {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<i64>,
    /// Generic payload for voice signaling messages (Stage 2+).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

// ─── Shared event queue ───────────────────────────────────────────────────────

pub type WsEventQueue = Arc<Mutex<VecDeque<WsServerEvent>>>;

pub fn new_event_queue() -> WsEventQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

// ─── Background WS task ───────────────────────────────────────────────────────

/// Runs in a background thread, maintains the WS connection.
/// Pushes received events to `event_queue` and calls `ctx.request_repaint()`.
/// Exits when `sender` is dropped (channel closed).
pub async fn ws_task(
    ws_url: String,
    event_queue: WsEventQueue,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<WsClientMsg>,
    ctx: egui::Context,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let conn = tokio_tungstenite::connect_async(&ws_url).await;
    let (ws_stream, _) = match conn {
        Ok(c) => c,
        Err(e) => {
            eprintln!("WS connect error: {e}");
            return;
        }
    };

    let (mut write, mut read) = ws_stream.split();

    // Heartbeat: send application-level ping every 25 s to keep the
    // TCP connection alive and reset any server-side read-idle timers.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(25));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(event) = serde_json::from_str::<WsServerEvent>(&text) {
                            event_queue.lock().push_back(event);
                            ctx.request_repaint();
                        }
                    }
                    // CRITICAL: respond to server Ping frames with Pong.
                    // nhooyr.io/websocket sends WebSocket Pings every ~25 s;
                    // if we don't Pong, the server closes the connection which
                    // triggers LeaveAll() and kicks us from the voice channel.
                    Some(Ok(Message::Ping(data))) => {
                        if write.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            outgoing = rx.recv() => {
                match outgoing {
                    Some(msg) => {
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if write.send(Message::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    None => break, // sender dropped → disconnect
                }
            }
            _ = heartbeat.tick() => {
                // Send a WebSocket-level Ping as extra keepalive insurance.
                if write.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        }
    }
}

// ─── API errors ───────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(String),
    #[error("unexpected status: {0}")]
    Status(u16),
    #[error("decode error: {0}")]
    Decode(String),
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_status() {
            if let Some(status) = e.status() {
                return ApiError::Status(status.as_u16());
            }
        }
        if e.is_decode() {
            return ApiError::Decode(e.to_string());
        }
        ApiError::Network(e.to_string())
    }
}

// ─── HTTP client ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ApiClient {
    pub base: String,
    http: reqwest::Client,
}

impl ApiClient {
    /// Таймаут для всех HTTP-запросов (неблокирующая загрузка, раздел 8 newUi.md).
    const REQUEST_TIMEOUT_SECS: u64 = 15;

    pub fn new(base: Option<String>) -> Self {
        let base = base.unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(Self::REQUEST_TIMEOUT_SECS))
            .build()
            .expect("http client");
        Self { base, http }
    }

    /// Derive WebSocket base URL from HTTP base.
    pub fn ws_base(&self) -> String {
        self.base
            .replace("https://", "wss://")
            .replace("http://", "ws://")
    }

    fn auth_header(token: &str) -> String {
        format!("Bearer {}", token)
    }

    // ── Auth ──────────────────────────────────────────────────────────────

    pub async fn register(&self, req: &RegisterRequest) -> Result<(), ApiError> {
        let url = format!("{}/auth/register", self.base);
        self.http
            .post(url)
            .json(req)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn login(&self, req: &LoginRequest) -> Result<TokenResponse, ApiError> {
        let url = format!("{}/auth/login", self.base);
        let resp = self
            .http
            .post(url)
            .json(req)
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;
        Ok(resp)
    }

    // ── Servers ───────────────────────────────────────────────────────────

    pub async fn list_servers(&self, token: &str) -> Result<Vec<Server>, ApiError> {
        let url = format!("{}/servers/", self.base);
        Ok(self
            .http
            .get(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Server>>()
            .await?)
    }

    pub async fn create_server(&self, token: &str, name: &str) -> Result<Server, ApiError> {
        let url = format!("{}/servers/", self.base);
        Ok(self
            .http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await?
            .error_for_status()?
            .json::<Server>()
            .await?)
    }

    pub async fn delete_server(&self, token: &str, server_id: i64) -> Result<(), ApiError> {
        let url = format!("{}/servers/{}", self.base, server_id);
        self.http
            .delete(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn rename_server(
        &self,
        token: &str,
        server_id: i64,
        name: &str,
    ) -> Result<Server, ApiError> {
        let url = format!("{}/servers/{}", self.base, server_id);
        Ok(self
            .http
            .patch(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await?
            .error_for_status()?
            .json::<Server>()
            .await?)
    }

    // ── Channels ──────────────────────────────────────────────────────────

    pub async fn list_channels(
        &self,
        token: &str,
        server_id: i64,
    ) -> Result<Vec<Channel>, ApiError> {
        let url = format!("{}/channels/?server_id={}", self.base, server_id);
        Ok(self
            .http
            .get(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Channel>>()
            .await?)
    }

    pub async fn create_channel(
        &self,
        token: &str,
        server_id: i64,
        name: &str,
        ch_type: &str,
    ) -> Result<Channel, ApiError> {
        let url = format!("{}/channels/", self.base);
        Ok(self
            .http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "server_id": server_id, "name": name, "type": ch_type }))
            .send()
            .await?
            .error_for_status()?
            .json::<Channel>()
            .await?)
    }

    pub async fn rename_channel(
        &self,
        token: &str,
        channel_id: i64,
        name: &str,
    ) -> Result<Channel, ApiError> {
        let url = format!("{}/channels/{}", self.base, channel_id);
        Ok(self
            .http
            .patch(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await?
            .error_for_status()?
            .json::<Channel>()
            .await?)
    }

    // ── Members ───────────────────────────────────────────────────────────

    pub async fn list_server_members(
        &self,
        token: &str,
        server_id: i64,
    ) -> Result<Vec<Member>, ApiError> {
        let url = format!("{}/members/?server_id={}", self.base, server_id);
        Ok(self
            .http
            .get(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Member>>()
            .await?)
    }

    pub async fn add_member(
        &self,
        token: &str,
        server_id: i64,
        user_id: i64,
    ) -> Result<(), ApiError> {
        let url = format!("{}/members/", self.base);
        self.http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "server_id": server_id, "user_id": user_id }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn set_nickname(
        &self,
        token: &str,
        server_id: i64,
        nickname: &str,
    ) -> Result<(), ApiError> {
        let url = format!("{}/members/nickname", self.base);
        self.http
            .patch(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "server_id": server_id, "nickname": nickname }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn kick_member(
        &self,
        token: &str,
        server_id: i64,
        user_id: i64,
    ) -> Result<(), ApiError> {
        let url = format!("{}/members/kick", self.base);
        self.http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "server_id": server_id, "user_id": user_id }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn ban_member(
        &self,
        token: &str,
        server_id: i64,
        user_id: i64,
    ) -> Result<(), ApiError> {
        let url = format!("{}/members/ban", self.base);
        self.http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "server_id": server_id, "user_id": user_id }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn list_server_bans(
        &self,
        token: &str,
        server_id: i64,
    ) -> Result<Vec<Member>, ApiError> {
        let url = format!("{}/members/bans?server_id={}", self.base, server_id);
        Ok(self
            .http
            .get(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Member>>()
            .await?)
    }

    pub async fn unban_member(
        &self,
        token: &str,
        server_id: i64,
        user_id: i64,
    ) -> Result<(), ApiError> {
        let url = format!("{}/members/ban/{}?server_id={}", self.base, user_id, server_id);
        self.http
            .delete(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn create_invite_link(
        &self,
        token: &str,
        server_id: i64,
        channel_id: Option<i64>,
    ) -> Result<InvitePreview, ApiError> {
        let url = format!("{}/invites/", self.base);
        Ok(self
            .http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "server_id": server_id, "channel_id": channel_id }))
            .send()
            .await?
            .error_for_status()?
            .json::<InvitePreview>()
            .await?)
    }

    pub async fn get_invite_preview(&self, token: &str) -> Result<InvitePreview, ApiError> {
        let url = format!("{}/invites/{}", self.base, token);
        Ok(self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json::<InvitePreview>()
            .await?)
    }

    pub async fn accept_invite(
        &self,
        token: &str,
        invite_token: &str,
    ) -> Result<InviteAcceptResponse, ApiError> {
        let url = format!("{}/invites/{}/accept", self.base, invite_token);
        Ok(self
            .http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?
            .json::<InviteAcceptResponse>()
            .await?)
    }

    // ── Messages ──────────────────────────────────────────────────────────

    pub async fn list_messages(
        &self,
        token: &str,
        channel_id: i64,
    ) -> Result<Vec<Message>, ApiError> {
        let url = format!("{}/messages/?channel_id={}", self.base, channel_id);
        Ok(self
            .http
            .get(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Message>>()
            .await?)
    }

    pub async fn send_message(
        &self,
        token: &str,
        channel_id: i64,
        content: &str,
        attachments: Vec<AttachmentMeta>,
    ) -> Result<Message, ApiError> {
        let url = format!("{}/messages/", self.base);
        Ok(self
            .http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({
                "channel_id": channel_id,
                "content": content,
                "attachments": attachments,
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<Message>()
            .await?)
    }

    // ── Avatar ────────────────────────────────────────────────────────────

    pub async fn get_avatar(&self, user_id: i64) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}/users/avatar?user_id={}", self.base, user_id);
        let resp = self.http.get(url).send().await?.error_for_status()?;
        Ok(resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ApiError::Network(e.to_string()))?)
    }

    pub async fn set_avatar(&self, token: &str, data: Vec<u8>, mime: &str) -> Result<(), ApiError> {
        let url = format!("{}/users/me/avatar", self.base);
        self.http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .header("Content-Type", mime)
            .body(data)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    // ── Media ─────────────────────────────────────────────────────────────

    pub async fn upload_media(
        &self,
        token: &str,
        server_id: i64,
        filename: &str,
        mime: &str,
        data: Vec<u8>,
    ) -> Result<MediaUploadResponse, ApiError> {
        let url = format!(
            "{}/media/?server_id={}&filename={}",
            self.base,
            server_id,
            urlencoding_encode(filename)
        );
        Ok(self
            .http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .header("Content-Type", mime)
            .body(data)
            .send()
            .await?
            .error_for_status()?
            .json::<MediaUploadResponse>()
            .await?)
    }

    pub async fn download_media(&self, token: &str, media_id: i64) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}/media/{}", self.base, media_id);
        let resp = self
            .http
            .get(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ApiError::Network(e.to_string()))?)
    }

    // ── Voice ─────────────────────────────────────────────────────────────

    /// Join a voice channel. Returns LiveKit URL, token, and current participant list.
    pub async fn voice_join(
        &self,
        token: &str,
        channel_id: i64,
        server_id: i64,
    ) -> Result<VoiceJoinResponse, ApiError> {
        let url = format!("{}/voice/join", self.base);
        Ok(self
            .http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "channel_id": channel_id, "server_id": server_id }))
            .send()
            .await?
            .error_for_status()?
            .json::<VoiceJoinResponse>()
            .await?)
    }

    /// Leave a voice channel.
    pub async fn voice_leave(&self, token: &str, channel_id: i64) -> Result<(), ApiError> {
        let url = format!("{}/voice/leave", self.base);
        self.http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({ "channel_id": channel_id }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Update mic/cam/streaming/deafened state in the current voice room.
    pub async fn voice_update_state(
        &self,
        token: &str,
        channel_id: i64,
        mic_muted: bool,
        deafened: bool,
        cam_enabled: bool,
        streaming: bool,
    ) -> Result<(), ApiError> {
        let url = format!("{}/voice/mute", self.base);
        self.http
            .post(url)
            .header("Authorization", Self::auth_header(token))
            .json(&serde_json::json!({
                "channel_id":  channel_id,
                "mic_muted":   mic_muted,
                "deafened":    deafened,
                "cam_enabled": cam_enabled,
                "streaming":   streaming,
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Fetch the current participant list for a voice channel (REST fallback).
    pub async fn voice_state(
        &self,
        token: &str,
        channel_id: i64,
    ) -> Result<Vec<VoiceParticipant>, ApiError> {
        let url = format!("{}/voice/state?channel_id={}", self.base, channel_id);
        #[derive(serde::Deserialize)]
        struct Resp {
            participants: Vec<VoiceParticipant>,
        }
        let r = self
            .http
            .get(url)
            .header("Authorization", Self::auth_header(token))
            .send()
            .await?
            .error_for_status()?
            .json::<Resp>()
            .await?;
        Ok(r.participants)
    }
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// Re-export egui for ws_task signature
use eframe::egui;
