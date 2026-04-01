package members

import (
	"encoding/json"
	"net/http"
	"strconv"

	"astrix/server/internal/auth"
	"astrix/server/internal/store"
	"astrix/server/internal/ws"

	"github.com/go-chi/chi/v5"
)

func memberJSON(member store.MemberRow) map[string]interface{} {
	return map[string]interface{}{
		"user_id":      member.UserID,
		"username":     member.Username,
		"display_name": member.DisplayName,
		"is_owner":     member.IsOwner,
	}
}

func Invite(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
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
		isMember, err := st.IsServerMember(r.Context(), body.ServerID, userID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if !isMember {
			http.Error(w, "server access denied", http.StatusForbidden)
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
		targetAlreadyMember, err := st.IsServerMember(r.Context(), body.ServerID, body.UserID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if targetAlreadyMember {
			http.Error(w, "user already in server", http.StatusConflict)
			return
		}
		banned, err := st.IsUserBanned(r.Context(), body.ServerID, body.UserID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if banned {
			http.Error(w, "user banned", http.StatusForbidden)
			return
		}
		if err := st.AddMemberToServer(r.Context(), body.ServerID, body.UserID); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		srv, err := st.GetServer(r.Context(), body.ServerID)
		if err == nil {
			hub.SendToUserAnywhere(body.ServerID, body.UserID, "server.added", map[string]interface{}{
				"id":        srv.ID,
				"server_id": srv.ID,
				"name":      srv.Name,
				"owner_id":  srv.OwnerID,
			})
		}
		// Broadcast new member to existing server members
		list, err := st.ListMembersForServer(r.Context(), body.ServerID)
		if err == nil {
			for _, m := range list {
				if m.UserID == body.UserID {
					hub.BroadcastToServer(body.ServerID, "member.joined", memberJSON(m))
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

func Kick(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
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

		ownerID, err := st.GetServerOwner(r.Context(), body.ServerID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if ownerID != userID {
			http.Error(w, "only server owner can kick members", http.StatusForbidden)
			return
		}
		if body.UserID == ownerID {
			http.Error(w, store.ErrCannotModerateOwner.Error(), http.StatusForbidden)
			return
		}

		if err := st.RemoveMemberFromServerAndPresence(r.Context(), body.ServerID, body.UserID); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		hub.BroadcastToServer(body.ServerID, "member.left", map[string]interface{}{
			"user_id":   body.UserID,
			"server_id": body.ServerID,
		})
		hub.SendToUserAnywhere(body.ServerID, body.UserID, "server.deleted", map[string]interface{}{
			"server_id": body.ServerID,
		})
		w.WriteHeader(http.StatusNoContent)
	}
}

func Ban(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
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

		ownerID, err := st.GetServerOwner(r.Context(), body.ServerID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if ownerID != userID {
			http.Error(w, "only server owner can ban members", http.StatusForbidden)
			return
		}
		if err := st.BanUserFromServer(r.Context(), body.ServerID, body.UserID, userID); err != nil {
			status := http.StatusInternalServerError
			if err == store.ErrCannotModerateOwner {
				status = http.StatusForbidden
			}
			http.Error(w, err.Error(), status)
			return
		}

		hub.BroadcastToServer(body.ServerID, "member.left", map[string]interface{}{
			"user_id":   body.UserID,
			"server_id": body.ServerID,
		})
		hub.SendToUserAnywhere(body.ServerID, body.UserID, "server.deleted", map[string]interface{}{
			"server_id": body.ServerID,
		})
		w.WriteHeader(http.StatusNoContent)
	}
}

func ListBans(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		serverID, err := strconv.ParseInt(r.URL.Query().Get("server_id"), 10, 64)
		if err != nil || serverID <= 0 {
			http.Error(w, "invalid server_id", http.StatusBadRequest)
			return
		}
		ownerID, err := st.GetServerOwner(r.Context(), serverID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if ownerID != userID {
			http.Error(w, "only server owner can view bans", http.StatusForbidden)
			return
		}
		list, err := st.ListServerBans(r.Context(), serverID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(list)
	}
}

func Unban(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		serverID, err := strconv.ParseInt(r.URL.Query().Get("server_id"), 10, 64)
		if err != nil || serverID <= 0 {
			http.Error(w, "invalid server_id", http.StatusBadRequest)
			return
		}
		targetUserID, err := strconv.ParseInt(chi.URLParam(r, "user_id"), 10, 64)
		if err != nil || targetUserID <= 0 {
			http.Error(w, "invalid user_id", http.StatusBadRequest)
			return
		}
		ownerID, err := st.GetServerOwner(r.Context(), serverID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if ownerID != userID {
			http.Error(w, "only server owner can unban users", http.StatusForbidden)
			return
		}
		if err := st.UnbanUserFromServer(r.Context(), serverID, targetUserID); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		w.WriteHeader(http.StatusNoContent)
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
