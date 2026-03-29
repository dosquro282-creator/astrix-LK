package channels

import (
	"encoding/json"
	"net/http"
	"strconv"

	"astrix/server/internal/store"
	"astrix/server/internal/ws"

	"github.com/go-chi/chi/v5"
)

func channelJSON(ch store.ChannelRow) map[string]interface{} {
	return map[string]interface{}{
		"id":        ch.ID,
		"server_id": ch.ServerID,
		"name":      ch.Name,
		"type":      ch.Type,
	}
}

func Create(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			ServerID int64  `json:"server_id"`
			Name     string `json:"name"`
			Type     string `json:"type"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			http.Error(w, "bad request", http.StatusBadRequest)
			return
		}
		if body.Name == "" || body.ServerID == 0 {
			http.Error(w, "name and server_id required", http.StatusBadRequest)
			return
		}
		if body.Type != "text" && body.Type != "voice" {
			body.Type = "text"
		}
		ch, err := st.CreateChannel(r.Context(), body.ServerID, body.Name, body.Type)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		// Broadcast to all server members so they see the new channel immediately
		hub.BroadcastToServer(ch.ServerID, "channel.created", channelJSON(ch))

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(channelJSON(ch))
	}
}

func List(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		serverIDStr := r.URL.Query().Get("server_id")
		serverID, err := strconv.ParseInt(serverIDStr, 10, 64)
		if err != nil || serverID <= 0 {
			http.Error(w, "invalid server_id", http.StatusBadRequest)
			return
		}
		list, err := st.ListChannelsForServer(r.Context(), serverID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		type channel struct {
			ID       int64  `json:"id"`
			ServerID int64  `json:"server_id"`
			Name     string `json:"name"`
			Type     string `json:"type"`
		}
		out := make([]channel, len(list))
		for i := range list {
			out[i] = channel{
				ID:       list[i].ID,
				ServerID: list[i].ServerID,
				Name:     list[i].Name,
				Type:     list[i].Type,
			}
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(out)
	}
}

// Rename handles PATCH /channels/{id}
func Rename(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		channelIDStr := chi.URLParam(r, "id")
		channelID, err := strconv.ParseInt(channelIDStr, 10, 64)
		if err != nil || channelID <= 0 {
			http.Error(w, "invalid channel id", http.StatusBadRequest)
			return
		}
		var body struct {
			Name string `json:"name"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil || body.Name == "" {
			http.Error(w, "name required", http.StatusBadRequest)
			return
		}
		ch, err := st.RenameChannel(r.Context(), channelID, body.Name)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		hub.BroadcastToServer(ch.ServerID, "channel.renamed", channelJSON(ch))

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(channelJSON(ch))
	}
}
