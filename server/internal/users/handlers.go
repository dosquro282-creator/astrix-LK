package users

import (
	"bytes"
	"image"
	_ "image/gif"
	"image/jpeg"
	_ "image/png"
	"io"
	"net/http"
	"strconv"

	"astrix/server/internal/auth"
	"astrix/server/internal/store"
	"astrix/server/internal/ws"
)

const maxAvatarSize = 256
const jpegQuality = 85

// GetAvatar serves the avatar bytes for a given user.
func GetAvatar(st *store.Store) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userIDStr := r.URL.Query().Get("user_id")
		userID, err := strconv.ParseInt(userIDStr, 10, 64)
		if err != nil || userID <= 0 {
			http.Error(w, "invalid user_id", http.StatusBadRequest)
			return
		}
		data, err := st.GetAvatar(r.Context(), userID)
		if err != nil || len(data) == 0 {
			http.Error(w, "not found", http.StatusNotFound)
			return
		}
		w.Header().Set("Content-Type", "image/jpeg")
		w.Header().Set("Cache-Control", "public, max-age=3600") // 1h cache to reduce repeated fetches
		_, _ = w.Write(data)
	}
}

// resizeAvatar decodes the image, resizes to max maxAvatarSize×maxAvatarSize if larger,
// and re-encodes as JPEG. Returns original data if decode fails or image is already small.
func resizeAvatar(data []byte) []byte {
	img, _, err := image.Decode(bytes.NewReader(data))
	if err != nil {
		return data
	}
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	if w <= maxAvatarSize && h <= maxAvatarSize {
		return data
	}
	newW, newH := w, h
	if w > h {
		newW = maxAvatarSize
		newH = h * maxAvatarSize / w
		if newH < 1 {
			newH = 1
		}
	} else {
		newH = maxAvatarSize
		newW = w * maxAvatarSize / h
		if newW < 1 {
			newW = 1
		}
	}
	dst := image.NewRGBA(image.Rect(0, 0, newW, newH))
	for y := 0; y < newH; y++ {
		for x := 0; x < newW; x++ {
			srcX := b.Min.X + x*w/newW
			if srcX >= b.Max.X {
				srcX = b.Max.X - 1
			}
			srcY := b.Min.Y + y*h/newH
			if srcY >= b.Max.Y {
				srcY = b.Max.Y - 1
			}
			dst.Set(x, y, img.At(srcX, srcY))
		}
	}
	var buf bytes.Buffer
	if err := jpeg.Encode(&buf, dst, &jpeg.Options{Quality: jpegQuality}); err != nil {
		return data
	}
	return buf.Bytes()
}

// SetAvatar stores the avatar for the authenticated user.
// Body: raw image bytes. Content-Type header used as mime type.
// Resizes to max 256×256 to reduce storage and network load.
// Broadcasts user.updated to all servers the user is in so clients invalidate avatar cache.
func SetAvatar(st *store.Store, hub *ws.Hub) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		userID, ok := r.Context().Value(auth.UserIDKey).(int64)
		if !ok {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		data, err := io.ReadAll(io.LimitReader(r.Body, 5<<20)) // 5 MB limit
		if err != nil || len(data) == 0 {
			http.Error(w, "bad body", http.StatusBadRequest)
			return
		}
		data = resizeAvatar(data)
		if err := st.SetAvatar(r.Context(), userID, data); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		hub.BroadcastToUserServers(r.Context(), userID, "user.updated", map[string]interface{}{
			"user_id":        userID,
			"avatar_changed": true,
		})
		w.WriteHeader(http.StatusNoContent)
	}
}
