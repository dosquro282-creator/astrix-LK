package voice

import (
	"sync"

	"astrix/server/internal/ws"
)

// Manager maintains in-memory voice room state and broadcasts WS events.
// LiveKit handles media; this is only for participant list and presence sync (webhook is source of truth for DB).
type Manager struct {
	mu    sync.RWMutex
	rooms map[int64]*Room // key = channel_id
	hub   *ws.Hub
}

// Room holds participants of one voice channel.
type Room struct {
	ChannelID int64
	ServerID  int64
	mu        sync.RWMutex
	peers     map[int64]*Participant // key = user_id
}

// Participant represents a user inside a voice room.
type Participant struct {
	UserID     int64
	Username   string
	MicMuted   bool
	Deafened   bool
	CamEnabled bool
	Streaming  bool
}

// PeerInfo is the JSON-serialisable view of a Participant.
type PeerInfo struct {
	UserID     int64  `json:"user_id"`
	Username   string `json:"username"`
	MicMuted   bool   `json:"mic_muted"`
	Deafened   bool   `json:"deafened"`
	CamEnabled bool   `json:"cam_enabled"`
	Streaming  bool   `json:"streaming"`
	ChannelID  int64  `json:"channel_id,omitempty"`
}

// NewManager creates a Manager. No WebRTC; LiveKit handles media.
func NewManager(hub *ws.Hub) *Manager {
	return &Manager{
		rooms: make(map[int64]*Room),
		hub:   hub,
	}
}

// Join adds a participant to the voice room. Broadcasts voice.participant_joined. Returns room state.
func (m *Manager) Join(channelID, serverID, userID int64, username string) []PeerInfo {
	m.mu.Lock()
	room, ok := m.rooms[channelID]
	if !ok {
		room = &Room{
			ChannelID: channelID,
			ServerID:  serverID,
			peers:     make(map[int64]*Participant),
		}
		m.rooms[channelID] = room
	}
	m.mu.Unlock()

	room.mu.Lock()
	room.peers[userID] = &Participant{
		UserID:   userID,
		Username: username,
	}
	room.mu.Unlock()

	m.hub.BroadcastToServer(serverID, "voice.participant_joined", PeerInfo{
		UserID:    userID,
		Username:  username,
		ChannelID: channelID,
	})

	return m.roomState(channelID)
}

// Leave removes a participant and broadcasts voice.participant_left.
func (m *Manager) Leave(channelID, userID int64) {
	m.mu.RLock()
	room, ok := m.rooms[channelID]
	m.mu.RUnlock()
	if !ok {
		return
	}

	room.mu.Lock()
	_, ok = room.peers[userID]
	if ok {
		delete(room.peers, userID)
	}
	empty := len(room.peers) == 0
	serverID := room.ServerID
	room.mu.Unlock()

	if ok {
		m.hub.BroadcastToServer(serverID, "voice.participant_left", map[string]interface{}{
			"user_id":    userID,
			"channel_id": channelID,
		})
	}

	if empty {
		m.mu.Lock()
		delete(m.rooms, channelID)
		m.mu.Unlock()
	}
}

// LeaveAll removes a user from every voice room (e.g. on WS disconnect).
func (m *Manager) LeaveAll(userID int64) {
	m.mu.RLock()
	var channels []int64
	for chID, room := range m.rooms {
		room.mu.RLock()
		_, in := room.peers[userID]
		room.mu.RUnlock()
		if in {
			channels = append(channels, chID)
		}
	}
	m.mu.RUnlock()

	for _, chID := range channels {
		m.Leave(chID, userID)
	}
}

// UpdateState updates mic/deafened/cam/streaming and broadcasts voice.state_update.
func (m *Manager) UpdateState(channelID, userID int64, micMuted, deafened, camEnabled, streaming bool) {
	m.mu.RLock()
	room, ok := m.rooms[channelID]
	m.mu.RUnlock()
	if !ok {
		return
	}

	room.mu.Lock()
	peer, ok := room.peers[userID]
	if ok {
		peer.MicMuted = micMuted
		peer.Deafened = deafened
		peer.CamEnabled = camEnabled
		peer.Streaming = streaming
	}
	room.mu.Unlock()

	if !ok {
		return
	}

	m.hub.BroadcastToServer(room.ServerID, "voice.state_update", map[string]interface{}{
		"user_id":     userID,
		"channel_id":  channelID,
		"mic_muted":   micMuted,
		"deafened":    deafened,
		"cam_enabled": camEnabled,
		"streaming":   streaming,
	})
}

// RoomState returns the current participant list for a channel.
func (m *Manager) RoomState(channelID int64) []PeerInfo {
	return m.roomState(channelID)
}

// UserRoom returns the channel_id of the voice room the user is in on the given server, or 0.
func (m *Manager) UserRoom(serverID, userID int64) int64 {
	m.mu.RLock()
	defer m.mu.RUnlock()
	for chID, room := range m.rooms {
		if room.ServerID != serverID {
			continue
		}
		room.mu.RLock()
		_, in := room.peers[userID]
		room.mu.RUnlock()
		if in {
			return chID
		}
	}
	return 0
}

func (m *Manager) roomState(channelID int64) []PeerInfo {
	m.mu.RLock()
	room, ok := m.rooms[channelID]
	m.mu.RUnlock()
	if !ok {
		return []PeerInfo{}
	}
	room.mu.RLock()
	defer room.mu.RUnlock()
	out := make([]PeerInfo, 0, len(room.peers))
	for _, p := range room.peers {
		out = append(out, PeerInfo{
			UserID:     p.UserID,
			Username:   p.Username,
			MicMuted:   p.MicMuted,
			Deafened:   p.Deafened,
			CamEnabled: p.CamEnabled,
			Streaming:  p.Streaming,
		})
	}
	return out
}
