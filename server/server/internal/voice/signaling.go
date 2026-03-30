package voice

import (
	"context"
	"encoding/json"
	"log"
	"net/http"
	"strconv"

	"astrix/server/internal/auth"
	"astrix/server/internal/config"
	"astrix/server/internal/store"

	"github.com/go-chi/chi/v5"
)

// livekitURLForClient normalizes LIVEKIT_URL so the client gets ws:// or wss://.
func livekitURLForClient(url string) string {
	if len(url) >= 7 && url[0:7] == "http://" {
		return "ws://" + url[7:]
	}
	if len(url) >= 8 && url[0:8] == "https://" {
		return "wss://" + url[8:]
	}
	return url
}

// RegisterRoutes wires all /voice/* endpoints into the provided router.
// The router is expected to be behind the auth middleware.
func RegisterRoutes(r chi.Router, mgr *Manager, st *store.Store, cfg config.Config) {
	// POST /voice/join
	// Body: { "channel_id": N, "server_id": N }
	// Response: { "livekit_url": "...", "token": "...", "participants": [...] }
	// Membership is checked via store; presence source of truth is LiveKit webhook.
	r.Post("/join", func(w http.ResponseWriter, r *http.Request) {
		userID, _ := mustUser(r)

		var body struct {
			ChannelID int64 `json:"channel_id"`
			ServerID  int64 `json:"server_id"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil ||
			body.ChannelID == 0 || body.ServerID == 0 {
			http.Error(w, "channel_id and server_id required", http.StatusBadRequest)
			return
		}

		// Verify user is a member of the server (and get display name).
		displayName, err := st.GetDisplayName(r.Context(), body.ServerID, userID)
		if err != nil {
			http.Error(w, "not a member of this server", http.StatusForbidden)
			return
		}

		roomName := LiveKitRoomName(body.ChannelID)
		identity := strconv.FormatInt(userID, 10) // numeric user_id, no prefix
		token, err := CreateLiveKitToken(roomName, identity, displayName, cfg)
		if err != nil {
			log.Printf("voice join: create livekit token: %v", err)
			http.Error(w, "failed to create token", http.StatusInternalServerError)
			return
		}

		// Optimistic: update in-memory room and DB + broadcast; webhook is source of truth.
		if prev := mgr.UserRoom(body.ServerID, userID); prev != 0 && prev != body.ChannelID {
			mgr.Leave(prev, userID)
			_ = st.VoiceLeave(r.Context(), prev, userID)
		}
		participants := mgr.Join(body.ChannelID, body.ServerID, userID, displayName)
		_ = st.VoiceJoin(r.Context(), body.ChannelID, userID)

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]interface{}{
			"livekit_url":  livekitURLForClient(cfg.LiveKitURL),
			"token":        token,
			"participants": participants,
		})
	})

	// POST /voice/leave
	// Body: { "channel_id": N }
	r.Post("/leave", func(w http.ResponseWriter, r *http.Request) {
		userID, _ := mustUser(r)

		var body struct {
			ChannelID int64 `json:"channel_id"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil || body.ChannelID == 0 {
			http.Error(w, "channel_id required", http.StatusBadRequest)
			return
		}

		mgr.Leave(body.ChannelID, userID)
		_ = st.VoiceLeave(r.Context(), body.ChannelID, userID)

		w.WriteHeader(http.StatusNoContent)
	})

	// POST /voice/mute  — update mic / deafened / cam / streaming state
	// Body: { "channel_id": N, "mic_muted": bool, "deafened": bool, "cam_enabled": bool, "streaming": bool }
	r.Post("/mute", func(w http.ResponseWriter, r *http.Request) {
		userID, _ := mustUser(r)

		var body struct {
			ChannelID  int64 `json:"channel_id"`
			MicMuted   bool  `json:"mic_muted"`
			Deafened   bool  `json:"deafened"`
			CamEnabled bool  `json:"cam_enabled"`
			Streaming  bool  `json:"streaming"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil || body.ChannelID == 0 {
			http.Error(w, "channel_id required", http.StatusBadRequest)
			return
		}

		mgr.UpdateState(
			body.ChannelID,
			userID,
			body.MicMuted,
			body.Deafened,
			body.CamEnabled,
			body.Streaming,
		)
		_ = st.VoiceUpdateState(r.Context(), body.ChannelID, userID,
			body.MicMuted, body.Deafened, body.CamEnabled, body.Streaming)

		w.WriteHeader(http.StatusNoContent)
	})

	// GET /voice/state?channel_id=N
	// Response: { "participants": [...] }
	r.Get("/state", func(w http.ResponseWriter, r *http.Request) {
		channelIDStr := r.URL.Query().Get("channel_id")
		channelID, err := strconv.ParseInt(channelIDStr, 10, 64)
		if err != nil || channelID == 0 {
			http.Error(w, "channel_id required", http.StatusBadRequest)
			return
		}

		participants := mgr.RoomState(channelID)
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]interface{}{
			"participants": participants,
		})
	})
}

// mustUser extracts userID and username from the request context.
// Panics are recovered by chi's middleware.Recoverer.
func mustUser(r *http.Request) (int64, string) {
	userID, _ := r.Context().Value(auth.UserIDKey).(int64)
	username, _ := r.Context().Value(auth.UserNameKey).(string)
	return userID, username
}

// LeaveOnDisconnect returns a ws.Hub OnDisconnect callback that cleans up
// voice state when a WebSocket client drops.
func LeaveOnDisconnect(mgr *Manager, st *store.Store) func(serverID, userID int64) {
	return func(_ int64, userID int64) {
		// LeaveAll removes the user from every room they're in regardless of server.
		// We then sync the DB asynchronously.
		mgr.mu.RLock()
		var channels []int64
		for chID, room := range mgr.rooms {
			room.mu.RLock()
			_, in := room.peers[userID]
			room.mu.RUnlock()
			if in {
				channels = append(channels, chID)
			}
		}
		mgr.mu.RUnlock()

		for _, chID := range channels {
			mgr.Leave(chID, userID)
			_ = st.VoiceLeave(context.Background(), chID, userID)
		}
	}
}
