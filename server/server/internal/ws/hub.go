package ws

import (
	"context"
	"encoding/json"
	"log"
	"net/http"
	"strconv"
	"sync"
	"time"

	"astrix/server/internal/auth"
	"astrix/server/internal/store"

	"github.com/go-chi/chi/v5"
	"github.com/golang-jwt/jwt/v5"
	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

// ServerEvent is sent from Hub → Client.
type ServerEvent struct {
	Type      string          `json:"type"`
	ServerID  int64           `json:"server_id,omitempty"`
	ChannelID int64           `json:"channel_id,omitempty"`
	Payload   json.RawMessage `json:"payload"`
}

// ClientMsg is sent from Client → Hub.
type ClientMsg struct {
	Type      string          `json:"type"`
	ChannelID int64           `json:"channel_id,omitempty"`
	Payload   json.RawMessage `json:"payload,omitempty"`
}

// BroadcastReq is an internal request to the Hub's run loop.
type BroadcastReq struct {
	ServerID      int64       // if > 0 && ChannelID == 0 → broadcast to all on server
	ChannelID     int64       // if > 0 → broadcast to clients viewing this channel
	EventType     string
	Payload       interface{}
	ExcludeUserID int64 // do not send to this user (0 = send to all)
}

type Hub struct {
	store       *store.Store
	mu          sync.RWMutex
	clients     map[*Client]bool
	register    chan *Client
	unregister  chan *Client
	broadcastCh chan BroadcastReq
	// OnDisconnect is called when a WS client disconnects.
	// Used by the voice layer to auto-leave voice rooms on WS drop.
	OnDisconnect func(serverID, userID int64)
}

type Client struct {
	hub       *Hub
	conn      *websocket.Conn
	userID    int64
	username  string
	serverID  int64
	channelID int64 // currently viewing channel (0 = none)
	send      chan ServerEvent
	mu        sync.Mutex
}

func NewHub(st *store.Store) *Hub {
	return &Hub{
		store:       st,
		clients:     make(map[*Client]bool),
		register:    make(chan *Client),
		unregister:  make(chan *Client),
		broadcastCh: make(chan BroadcastReq, 256),
	}
}

// BroadcastToServer sends an event to all clients connected to a server.
func (h *Hub) BroadcastToServer(serverID int64, eventType string, payload interface{}) {
	h.broadcastCh <- BroadcastReq{ServerID: serverID, EventType: eventType, Payload: payload}
}

// BroadcastToServerExcept broadcasts to server but skips excludeUID.
func (h *Hub) BroadcastToServerExcept(serverID int64, eventType string, payload interface{}, excludeUID int64) {
	h.broadcastCh <- BroadcastReq{ServerID: serverID, EventType: eventType, Payload: payload, ExcludeUserID: excludeUID}
}

// BroadcastToChannel sends to all clients currently viewing a channel.
func (h *Hub) BroadcastToChannel(channelID int64, eventType string, payload interface{}) {
	h.broadcastCh <- BroadcastReq{ChannelID: channelID, EventType: eventType, Payload: payload}
}

// SendToUser delivers an event directly to a specific user's WS connection on
// the given server.
func (h *Hub) SendToUser(serverID, userID int64, eventType string, payload interface{}) {
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
		if c.serverID == serverID && c.userID == userID {
			select {
			case c.send <- ev:
			default:
			}
			return
		}
	}
}

// BroadcastToChannelExcept broadcasts to channel but skips excludeUID.
func (h *Hub) BroadcastToChannelExcept(channelID int64, eventType string, payload interface{}, excludeUID int64) {
	h.broadcastCh <- BroadcastReq{ChannelID: channelID, EventType: eventType, Payload: payload, ExcludeUserID: excludeUID}
}

// BroadcastToUserServers sends an event to all servers the user is a member of.
// Used for user profile updates (e.g. avatar change) so all relevant clients get the update.
func (h *Hub) BroadcastToUserServers(ctx context.Context, userID int64, eventType string, payload interface{}) {
	servers, err := h.store.ListServersForUser(ctx, userID)
	if err != nil {
		return
	}
	for _, s := range servers {
		h.BroadcastToServer(s.ID, eventType, payload)
	}
}

// ViewersOfChannel returns user_ids of clients currently viewing a channel.
func (h *Hub) ViewersOfChannel(channelID int64) []int64 {
	h.mu.RLock()
	defer h.mu.RUnlock()
	var ids []int64
	for c := range h.clients {
		if c.channelID == channelID {
			ids = append(ids, c.userID)
		}
	}
	return ids
}

// OnlineUsersForServer returns unique user_ids of connected clients on a server.
func (h *Hub) OnlineUsersForServer(serverID int64) []int64 {
	h.mu.RLock()
	defer h.mu.RUnlock()
	seen := make(map[int64]bool)
	var ids []int64
	for c := range h.clients {
		if c.serverID == serverID && !seen[c.userID] {
			seen[c.userID] = true
			ids = append(ids, c.userID)
		}
	}
	return ids
}

func (h *Hub) Run() {
	for {
		select {
		case c := <-h.register:
			h.mu.Lock()
			h.clients[c] = true
			h.mu.Unlock()
			// Announce presence to others
			p, _ := json.Marshal(map[string]interface{}{
				"user_id":  c.userID,
				"username": c.username,
				"online":   true,
			})
			h.broadcastCh <- BroadcastReq{
				ServerID:      c.serverID,
				EventType:     "presence.update",
				Payload:       json.RawMessage(p),
				ExcludeUserID: c.userID,
			}
			// Send current online list to the newly connected client
			go func(cl *Client) {
				time.Sleep(50 * time.Millisecond)
				online := h.OnlineUsersForServer(cl.serverID)
				p2, _ := json.Marshal(map[string]interface{}{
					"online_user_ids": online,
				})
				raw, _ := marshalPayload(json.RawMessage(p2))
				ev := ServerEvent{Type: "presence.init", ServerID: cl.serverID, Payload: raw}
				select {
				case cl.send <- ev:
				default:
				}
			}(c)

		case c := <-h.unregister:
			h.mu.Lock()
			if _, ok := h.clients[c]; ok {
				delete(h.clients, c)
				close(c.send)
			}
			h.mu.Unlock()
			// Notify voice layer so it can clean up the participant's room.
			if h.OnDisconnect != nil {
				go h.OnDisconnect(c.serverID, c.userID)
			}
			p, _ := json.Marshal(map[string]interface{}{
				"user_id":  c.userID,
				"username": c.username,
				"online":   false,
			})
			h.broadcastCh <- BroadcastReq{
				ServerID:  c.serverID,
				EventType: "presence.update",
				Payload:   json.RawMessage(p),
			}

		case req := <-h.broadcastCh:
			payloadBytes, err := marshalPayload(req.Payload)
			if err != nil {
				log.Printf("ws marshal error: %v", err)
				continue
			}
			event := ServerEvent{
				Type:      req.EventType,
				ServerID:  req.ServerID,
				ChannelID: req.ChannelID,
				Payload:   payloadBytes,
			}
			h.mu.RLock()
			for c := range h.clients {
				if req.ExcludeUserID != 0 && c.userID == req.ExcludeUserID {
					continue
				}
				var match bool
				if req.ChannelID > 0 {
					match = c.channelID == req.ChannelID
				} else if req.ServerID > 0 {
					match = c.serverID == req.ServerID
				}
				if match {
					select {
					case c.send <- event:
					default:
						// slow client; skip
					}
				}
			}
			h.mu.RUnlock()
		}
	}
}

func marshalPayload(payload interface{}) (json.RawMessage, error) {
	if rm, ok := payload.(json.RawMessage); ok {
		return rm, nil
	}
	b, err := json.Marshal(payload)
	return b, err
}

func RegisterRoutes(r chi.Router, hub *Hub, authSvc *auth.Service) {
	r.Get("/ws", func(w http.ResponseWriter, req *http.Request) {
		ctx := req.Context()

		tokenStr := req.URL.Query().Get("token")
		if tokenStr == "" {
			http.Error(w, "missing token", http.StatusUnauthorized)
			return
		}

		token, err := authSvc.ParseToken(tokenStr)
		if err != nil || !token.Valid {
			http.Error(w, "invalid token", http.StatusUnauthorized)
			return
		}

		claims, ok := token.Claims.(jwt.MapClaims)
		if !ok {
			http.Error(w, "invalid token claims", http.StatusUnauthorized)
			return
		}
		sub, ok := claims["sub"].(float64)
		if !ok {
			http.Error(w, "invalid token subject", http.StatusUnauthorized)
			return
		}
		userID := int64(sub)
		username, _ := claims["usr"].(string)

		serverIDStr := req.URL.Query().Get("server_id")
		if serverIDStr == "" {
			http.Error(w, "missing server_id", http.StatusBadRequest)
			return
		}
		serverID, err := strconv.ParseInt(serverIDStr, 10, 64)
		if err != nil || serverID <= 0 {
			http.Error(w, "bad server_id", http.StatusBadRequest)
			return
		}

		channelID := int64(0)
		if chStr := req.URL.Query().Get("channel_id"); chStr != "" {
			channelID, _ = strconv.ParseInt(chStr, 10, 64)
		}

		conn, err := websocket.Accept(w, req, &websocket.AcceptOptions{
			OriginPatterns: []string{"*"},
		})
		if err != nil {
			log.Printf("ws accept error: %v", err)
			return
		}

		client := &Client{
			hub:       hub,
			conn:      conn,
			userID:    userID,
			username:  username,
			serverID:  serverID,
			channelID: channelID,
			send:      make(chan ServerEvent, 64),
		}
		hub.register <- client

		go client.writePump()
		client.readPump(ctx)
	})
}

func (c *Client) readPump(ctx context.Context) {
	defer func() {
		c.hub.unregister <- c
		_ = c.conn.Close(websocket.StatusNormalClosure, "closing")
	}()

	for {
		var msg ClientMsg
		if err := wsjson.Read(ctx, c.conn, &msg); err != nil {
			log.Printf("ws read error for user %d: %v", c.userID, err)
			break
		}
		switch msg.Type {
		case "channel.view":
			c.mu.Lock()
			c.channelID = msg.ChannelID
			c.mu.Unlock()
			if msg.ChannelID > 0 {
				// Persist read receipt when client sends last_message_id (after loading messages).
				if len(msg.Payload) > 0 {
					var body struct {
						LastMessageID int64 `json:"last_message_id"`
					}
					if _ = json.Unmarshal(msg.Payload, &body); body.LastMessageID > 0 {
						_ = c.hub.store.SetChannelRead(ctx, c.userID, msg.ChannelID, body.LastMessageID)
					}
				}
				// Notify other clients for real-time seen_by updates.
				p, _ := json.Marshal(map[string]interface{}{
					"reader_id":  c.userID,
					"channel_id": msg.ChannelID,
				})
				c.hub.BroadcastToServerExcept(c.serverID, "messages.read", json.RawMessage(p), c.userID)
			}
		case "typing":
			if msg.ChannelID == 0 {
				break
			}
			p, _ := json.Marshal(map[string]interface{}{
				"user_id":  c.userID,
				"username": c.username,
			})
			c.hub.BroadcastToChannelExcept(msg.ChannelID, "typing", json.RawMessage(p), c.userID)
		}
	}
}

func (c *Client) writePump() {
	ctx := context.Background()
	for event := range c.send {
		if err := wsjson.Write(ctx, c.conn, event); err != nil {
			log.Printf("ws write error: %v", err)
			return
		}
	}
}
