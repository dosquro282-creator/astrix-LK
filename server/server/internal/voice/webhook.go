package voice

import (
	"context"
	"log"
	"net/http"
	"strconv"
	"strings"

	"astrix/server/internal/config"
	"astrix/server/internal/store"

	"github.com/livekit/protocol/webhook"
)

// livekitKeyProvider returns the API secret for webhook signature verification.
type livekitKeyProvider struct {
	key    string
	secret string
}

func (p *livekitKeyProvider) GetSecret(key string) string {
	if key == p.key {
		return p.secret
	}
	return ""
}

func (p *livekitKeyProvider) NumKeys() int {
	return 1
}

// parseChannelIDFromRoomName extracts channel_id from room name "channel_<id>". Returns 0, false on parse error.
func parseChannelIDFromRoomName(roomName string) (int64, bool) {
	const prefix = "channel_"
	if !strings.HasPrefix(roomName, prefix) {
		return 0, false
	}
	id, err := strconv.ParseInt(roomName[len(prefix):], 10, 64)
	return id, err == nil && id > 0
}

// WebhookHandler returns the HTTP handler for LiveKit webhooks (participant_joined, participant_left).
// Must be registered without auth middleware. Source of truth for voice_presence; mgr.Join/Leave do WS broadcast.
func WebhookHandler(mgr *Manager, st *store.Store, cfg config.Config) http.HandlerFunc {
	kp := &livekitKeyProvider{key: cfg.LiveKitAPIKey, secret: cfg.LiveKitSecret}

	return func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}

		event, err := webhook.ReceiveWebhookEvent(r, kp)
		if err != nil {
			log.Printf("voice webhook: verify error: %v", err)
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}

		channelID, ok := parseChannelIDFromRoomName(event.Room.GetName())
		if !ok {
			log.Printf("voice webhook: invalid room name %q", event.Room.GetName())
			w.WriteHeader(http.StatusOK)
			return
		}

		ch, err := st.GetChannelByID(r.Context(), channelID)
		if err != nil {
			log.Printf("voice webhook: channel %d not found: %v", channelID, err)
			w.WriteHeader(http.StatusOK)
			return
		}
		serverID := ch.ServerID

		ctx := context.Background()

		switch event.Event {
		case "participant_joined":
			identity := event.Participant.GetIdentity()
			userID, err := strconv.ParseInt(identity, 10, 64)
			if err != nil || userID <= 0 {
				log.Printf("voice webhook: participant_joined invalid identity %q", identity)
				w.WriteHeader(http.StatusOK)
				return
			}
			name := event.Participant.GetName()
			if name == "" {
				name = identity
			}
			if err := st.VoiceJoin(ctx, channelID, userID); err != nil {
				log.Printf("voice webhook: VoiceJoin: %v", err)
			}
			mgr.Join(channelID, serverID, userID, name)
		case "participant_left":
			identity := event.Participant.GetIdentity()
			userID, err := strconv.ParseInt(identity, 10, 64)
			if err != nil || userID <= 0 {
				log.Printf("voice webhook: participant_left invalid identity %q", identity)
				w.WriteHeader(http.StatusOK)
				return
			}
			mgr.Leave(channelID, userID)
			if err := st.VoiceLeave(ctx, channelID, userID); err != nil {
				log.Printf("voice webhook: VoiceLeave: %v", err)
			}
		case "room_finished":
			// Optional: clean up any leftover presence for this channel if needed.
		default:
			// Ignore other events (track_published, etc.)
		}

		w.WriteHeader(http.StatusOK)
	}
}
