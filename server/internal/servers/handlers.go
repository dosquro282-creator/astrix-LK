package servers

import (
	"encoding/json"
	"net/http"
	"strconv"

	"astrix/server/internal/auth"
	"astrix/server/internal/store"
	"astrix/server/internal/ws"

	"github.com/go-chi/chi/v5"
)

func serverJSON(srv store.ServerRow) map[string]interface{} {
	return map[string]interface{}{
		"id":        srv.ID,
		"server_id": srv.ID,
		"name":      srv.Name,
		"owner_id":  srv.OwnerID,
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
			Name string `json:"name"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			http.Error(w, "bad request", http.StatusBadRequest)
			return
		}
		if body.Name == "" {
			http.Error(w, "name required", http.StatusBadRequest)
			return
		}
		srv, err := st.CreateServer(r.Context(), userID, body.Name)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(serverJSON(srv))
	}
}

func Delete(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		serverID, err := strconv.ParseInt(chi.URLParam(r, "id"), 10, 64)
		if err != nil || serverID <= 0 {
			http.Error(w, "invalid server id", http.StatusBadRequest)
			return
		}
		count, err := st.CountServerMembers(r.Context(), serverID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if count <= 1 {
			if err := st.DeleteServer(r.Context(), serverID); err != nil {
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			hub.BroadcastToServer(serverID, "server.deleted", map[string]interface{}{
				"server_id": serverID,
			})
		} else {
			if err := st.LeaveServer(r.Context(), serverID, userID); err != nil {
				http.Error(w, err.Error(), http.StatusInternalServerError)
				return
			}
			hub.BroadcastToServer(serverID, "member.left", map[string]interface{}{
				"user_id":   userID,
				"server_id": serverID,
			})
		}
		w.WriteHeader(http.StatusNoContent)
	}
}

func Update(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		serverID, err := strconv.ParseInt(chi.URLParam(r, "id"), 10, 64)
		if err != nil || serverID <= 0 {
			http.Error(w, "invalid server id", http.StatusBadRequest)
			return
		}

		ownerID, err := st.GetServerOwner(r.Context(), serverID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		if ownerID != userID {
			http.Error(w, "only server owner can update server", http.StatusForbidden)
			return
		}

		var body struct {
			Name string `json:"name"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil || body.Name == "" {
			http.Error(w, "name required", http.StatusBadRequest)
			return
		}

		srv, err := st.RenameServer(r.Context(), serverID, body.Name)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		payload := serverJSON(srv)
		hub.BroadcastToServerMembersAnywhere(r.Context(), serverID, "server.updated", payload)

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(payload)
	}
}

func List(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		list, err := st.ListServersForUser(r.Context(), userID)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		type server struct {
			ID      int64  `json:"id"`
			Name    string `json:"name"`
			OwnerID int64  `json:"owner_id"`
		}
		out := make([]server, len(list))
		for i := range list {
			out[i] = server{ID: list[i].ID, Name: list[i].Name, OwnerID: list[i].OwnerID}
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(out)
	}
}
