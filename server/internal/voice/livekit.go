package voice

import (
	"fmt"
	"time"

	"astrix/server/internal/config"

	"github.com/livekit/protocol/auth"
)

// LiveKitRoomName returns the room name for a voice channel: "channel_<channelID>".
func LiveKitRoomName(channelID int64) string {
	return fmt.Sprintf("channel_%d", channelID)
}

// CreateLiveKitToken builds a JWT for joining a LiveKit room.
// identity must be the string form of user_id (no prefix). name is display name.
func CreateLiveKitToken(roomName, identity, name string, cfg config.Config) (string, error) {
	at := auth.NewAccessToken(cfg.LiveKitAPIKey, cfg.LiveKitSecret)
	grant := &auth.VideoGrant{
		RoomJoin: true,
		Room:     roomName,
	}
	at.SetVideoGrant(grant).
		SetIdentity(identity).
		SetName(name).
		SetValidFor(24 * time.Hour)
	return at.ToJWT()
}
