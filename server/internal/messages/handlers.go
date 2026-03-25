package messages

import (
	"encoding/json"
	"net/http"
	"strconv"

	"astrix/server/internal/auth"
	"astrix/server/internal/store"
	"astrix/server/internal/ws"
)

func messageJSON(msg store.MessageRow, seenBy []int64) map[string]interface{} {
	attachments := msg.Attachments
	if attachments == nil {
		attachments = []store.AttachmentMeta{}
	}
	seenOut := seenBy
	if seenOut == nil {
		seenOut = []int64{}
	}
	return map[string]interface{}{
		"id":              msg.ID,
		"channel_id":      msg.ChannelID,
		"author_id":       msg.AuthorID,
		"author_username": msg.AuthorUsername,
		"content":         msg.Content,
		"created_at":      msg.CreatedAt,
		"attachments":     attachments,
		"seen_by":         seenOut,
	}
}

func Create(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		var body struct {
			ChannelID   int64                  `json:"channel_id"`
			Content     string                 `json:"content"`
			Attachments []store.AttachmentMeta `json:"attachments"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			http.Error(w, "bad request", http.StatusBadRequest)
			return
		}
		if body.ChannelID == 0 {
			http.Error(w, "channel_id required", http.StatusBadRequest)
			return
		}
		if body.Content == "" && len(body.Attachments) == 0 {
			http.Error(w, "content or attachments required", http.StatusBadRequest)
			return
		}
		msg, err := st.CreateMessage(r.Context(), body.ChannelID, userID, body.Content, body.Attachments)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		// Determine who is viewing the channel right now (for read receipt)
		viewers := hub.ViewersOfChannel(body.ChannelID)
		var seenBy []int64
		for _, uid := range viewers {
			if uid != userID {
				seenBy = append(seenBy, uid)
			}
		}
		// Broadcast to channel; client deduplicates by message ID
		hub.BroadcastToChannel(body.ChannelID, "message.created", messageJSON(msg, seenBy))

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(messageJSON(msg, seenBy))
	}
}

func List(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		_, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		channelID, err := strconv.ParseInt(r.URL.Query().Get("channel_id"), 10, 64)
		if err != nil || channelID <= 0 {
			http.Error(w, "invalid channel_id", http.StatusBadRequest)
			return
		}
		list, err := st.ListMessagesForChannel(r.Context(), channelID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		out := make([]map[string]interface{}, len(list))
		for i := range list {
			seenBy := list[i].SeenBy
			if seenBy == nil {
				seenBy = []int64{}
			}
			out[i] = messageJSON(list[i], seenBy)
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(out)
	}
}
