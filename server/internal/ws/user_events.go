package ws

import (
	"context"
)

func (h *Hub) SendToUserAnywhere(serverID, userID int64, eventType string, payload interface{}) {
	raw, err := marshalPayload(payload)
	if err != nil {
		return
	}
	ev := ServerEvent{
		Type:     eventType,
		ServerID: serverID,
		Payload:  raw,
	}
	h.mu.RLock()
	defer h.mu.RUnlock()
	for c := range h.clients {
		if c.userID != userID {
			continue
		}
		select {
		case c.send <- ev:
		default:
		}
	}
}

func (h *Hub) BroadcastToServerMembersAnywhere(
	ctx context.Context,
	serverID int64,
	eventType string,
	payload interface{},
) {
	members, err := h.store.ListMembersForServer(ctx, serverID)
	if err != nil {
		return
	}
	for _, member := range members {
		h.SendToUserAnywhere(serverID, member.UserID, eventType, payload)
	}
}
