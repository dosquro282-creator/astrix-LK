package invites

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"

	"astrix/server/internal/auth"
	"astrix/server/internal/store"
	"astrix/server/internal/ws"

	"github.com/go-chi/chi/v5"
)

func inviteJSON(invite store.ServerInviteRow) map[string]interface{} {
	payload := map[string]interface{}{
		"token":       invite.Token,
		"server_id":   invite.ServerID,
		"server_name": invite.ServerName,
		"owner_id":    invite.OwnerID,
	}
	if invite.ChannelID != nil {
		payload["channel_id"] = *invite.ChannelID
	}
	if invite.ChannelName != nil {
		payload["channel_name"] = *invite.ChannelName
	}
	return payload
}

func Redirect() http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		token := chi.URLParam(r, "token")
		if token == "" {
			http.Error(w, "invite token required", http.StatusBadRequest)
			return
		}
		http.Redirect(w, r, fmt.Sprintf("astrix://invite/%s", token), http.StatusFound)
	}
}

func Get(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		token := chi.URLParam(r, "token")
		if token == "" {
			http.Error(w, "invite token required", http.StatusBadRequest)
			return
		}

		invite, err := st.GetServerInvite(r.Context(), token)
		if err != nil {
			if errors.Is(err, store.ErrInviteNotFound) {
				http.Error(w, err.Error(), http.StatusNotFound)
				return
			}
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(inviteJSON(invite))
	}
}

func Create(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}

		var body struct {
			ServerID  int64  `json:"server_id"`
			ChannelID *int64 `json:"channel_id"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			http.Error(w, "bad request", http.StatusBadRequest)
			return
		}
		if body.ServerID == 0 {
			http.Error(w, "server_id required", http.StatusBadRequest)
			return
		}

		member, err := st.IsServerMember(r.Context(), body.ServerID, userID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if !member {
			http.Error(w, store.ErrServerAccessDenied.Error(), http.StatusForbidden)
			return
		}

		invite, err := st.CreateServerInvite(r.Context(), body.ServerID, body.ChannelID, userID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(inviteJSON(invite))
	}
}

func Accept(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}

		token := chi.URLParam(r, "token")
		if token == "" {
			http.Error(w, "invite token required", http.StatusBadRequest)
			return
		}

		invite, err := st.AcceptServerInvite(r.Context(), token, userID)
		if err != nil {
			switch {
			case errors.Is(err, store.ErrInviteNotFound):
				http.Error(w, err.Error(), http.StatusNotFound)
			case errors.Is(err, store.ErrUserBanned):
				http.Error(w, err.Error(), http.StatusForbidden)
			default:
				http.Error(w, err.Error(), http.StatusInternalServerError)
			}
			return
		}

		serverRow, err := st.GetServer(r.Context(), invite.ServerID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		hub.SendToUserAnywhere(serverRow.ID, userID, "server.added", map[string]interface{}{
			"id":        serverRow.ID,
			"server_id": serverRow.ID,
			"name":      serverRow.Name,
			"owner_id":  serverRow.OwnerID,
		})

		list, err := st.ListMembersForServer(r.Context(), invite.ServerID)
		if err == nil {
			for _, member := range list {
				if member.UserID == userID {
					hub.BroadcastToServer(invite.ServerID, "member.joined", map[string]interface{}{
						"user_id":      member.UserID,
						"username":     member.Username,
						"display_name": member.DisplayName,
						"is_owner":     member.IsOwner,
					})
					break
				}
			}
		}

		response := map[string]interface{}{
			"server": map[string]interface{}{
				"id":       serverRow.ID,
				"name":     serverRow.Name,
				"owner_id": serverRow.OwnerID,
			},
		}
		if invite.ChannelID != nil {
			response["channel_id"] = *invite.ChannelID
		}

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(response)
	}
}
