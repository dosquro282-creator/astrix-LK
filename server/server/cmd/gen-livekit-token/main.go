// gen-livekit-token prints a LiveKit access token for manual testing (e.g. with livekit-cli).
// Usage: LIVEKIT_API_KEY=astrixkey LIVEKIT_API_SECRET=astrixsecret go run ./cmd/gen-livekit-token
// Then: livekit-cli join-room --url ws://localhost:7880 --token <token>
package main

import (
	"fmt"
	"os"
	"time"

	"github.com/livekit/protocol/auth"
)

func main() {
	key := os.Getenv("LIVEKIT_API_KEY")
	secret := os.Getenv("LIVEKIT_API_SECRET")
	if key == "" || secret == "" {
		fmt.Fprintln(os.Stderr, "Set LIVEKIT_API_KEY and LIVEKIT_API_SECRET")
		os.Exit(1)
	}
	at := auth.NewAccessToken(key, secret)
	at.SetVideoGrant(&auth.VideoGrant{RoomJoin: true, Room: "channel_1"}).
		SetIdentity("1").
		SetName("TestUser").
		SetValidFor(24 * time.Hour)
	token, err := at.ToJWT()
	if err != nil {
		fmt.Fprintf(os.Stderr, "token: %v\n", err)
		os.Exit(1)
	}
	fmt.Println(token)
}
