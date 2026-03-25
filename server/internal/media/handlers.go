package media

import (
	"encoding/json"
	"io"
	"net/http"
	"strconv"

	"astrix/server/internal/auth"
	"astrix/server/internal/store"

	"github.com/go-chi/chi/v5"
)

// Upload receives raw bytes and stores as media. Query params: server_id, filename.
// Content-Type header used as mime type.
func Upload(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}

		serverIDStr := r.URL.Query().Get("server_id")
		serverID, err := strconv.ParseInt(serverIDStr, 10, 64)
		if err != nil || serverID <= 0 {
			http.Error(w, "invalid server_id", http.StatusBadRequest)
			return
		}

		filename := r.URL.Query().Get("filename")
		if filename == "" {
			filename = "file"
		}

		mimeType := r.Header.Get("Content-Type")
		if mimeType == "" {
			mimeType = "application/octet-stream"
		}

		data, err := io.ReadAll(io.LimitReader(r.Body, 50<<20)) // 50 MB limit
		if err != nil || len(data) == 0 {
			http.Error(w, "bad body", http.StatusBadRequest)
			return
		}

		m, err := st.SaveMedia(r.Context(), userID, serverID, filename, mimeType, data)
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]interface{}{
			"id":         m.ID,
			"filename":   m.Filename,
			"mime_type":  m.MimeType,
			"size_bytes": m.SizeBytes,
		})
	}
}

// Download serves the media bytes.
func Download(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		mediaIDStr := chi.URLParam(r, "id")
		mediaID, err := strconv.ParseInt(mediaIDStr, 10, 64)
		if err != nil || mediaID <= 0 {
			http.Error(w, "invalid id", http.StatusBadRequest)
			return
		}

		m, err := st.GetMedia(r.Context(), mediaID)
		if err != nil {
			http.Error(w, "not found", http.StatusNotFound)
			return
		}

		w.Header().Set("Content-Type", m.MimeType)
		w.Header().Set("Content-Disposition", `attachment; filename="`+m.Filename+`"`)
		_, _ = w.Write(m.Data)
	}
}
