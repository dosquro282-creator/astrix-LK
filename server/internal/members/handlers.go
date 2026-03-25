package members

import (
	"encoding/json"
	"net/http"
	"strconv"

	"astrix/server/internal/auth"
	"astrix/server/internal/store"
	"astrix/server/internal/ws"
)

func Invite(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		_, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		var body struct {
			ServerID int64 `json:"server_id"`
			UserID   int64 `json:"user_id"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			http.Error(w, "bad request", http.StatusBadRequest)
			return
		}
		if body.ServerID == 0 || body.UserID == 0 {
			http.Error(w, "server_id and user_id required", http.StatusBadRequest)
			return
		}
		exists, err := st.UserExistsByID(r.Context(), body.UserID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if !exists {
			http.Error(w, "user not found", http.StatusNotFound)
			return
		}
		if err := st.AddMemberToServer(r.Context(), body.ServerID, body.UserID); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		// Broadcast new member to existing server members
		list, err := st.ListMembersForServer(r.Context(), body.ServerID)
		if err == nil {
			for _, m := range list {
				if m.UserID == body.UserID {
					hub.BroadcastToServer(body.ServerID, "member.joined", map[string]interface{}{
						"user_id":      m.UserID,
						"username":     m.Username,
						"display_name": m.DisplayName,
						"is_owner":     m.IsOwner,
					})
					break
				}
			}
		}
		w.WriteHeader(http.StatusNoContent)
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
		list, err := st.ListMembersForServer(r.Context(), serverID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		type member struct {
			UserID      int64  `json:"user_id"`
			Username    string `json:"username"`
			DisplayName string `json:"display_name"`
			IsOwner     bool   `json:"is_owner"`
		}
		out := make([]member, len(list))
		for i := range list {
			out[i] = member{
				UserID:      list[i].UserID,
				Username:    list[i].Username,
				DisplayName: list[i].DisplayName,
				IsOwner:     list[i].IsOwner,
			}
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(out)
	}
}

// SetNickname handles PATCH /members/nickname
func SetNickname(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		var body struct {
			ServerID int64  `json:"server_id"`
			Nickname string `json:"nickname"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil || body.ServerID == 0 {
			http.Error(w, "server_id required", http.StatusBadRequest)
			return
		}
		if err := st.SetServerNickname(r.Context(), body.ServerID, userID, body.Nickname); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		displayName, _ := st.GetDisplayName(r.Context(), body.ServerID, userID)
		hub.BroadcastToServer(body.ServerID, "member.renamed", map[string]interface{}{
			"user_id":      userID,
			"display_name": displayName,
		})
		w.WriteHeader(http.StatusNoContent)
	}
}
