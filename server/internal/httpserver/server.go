package httpserver

import (
	"context"
	"log"
	"net/http"

	"astrix/server/internal/auth"
	"astrix/server/internal/channels"
	"astrix/server/internal/config"
	"astrix/server/internal/media"
	"astrix/server/internal/members"
	"astrix/server/internal/messages"
	"astrix/server/internal/servers"
	"astrix/server/internal/store"
	"astrix/server/internal/users"
	"astrix/server/internal/voice"
	"astrix/server/internal/ws"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
)

type Server struct {
	Router *chi.Mux
}

func NewServer(cfg config.Config) *Server {
	ctx := context.Background()

	st, err := store.New(ctx, cfg.DatabaseURL)
	if err != nil {
		log.Fatalf("failed to init store: %v", err)
	}

	// Clear stale voice presence from previous server runs.
	if err := st.VoiceClearAll(ctx); err != nil {
		log.Printf("voice presence clear warning: %v", err)
	}

	authSvc := auth.NewService(st, []byte(cfg.JWTSecret))
	wsHub := ws.NewHub(st)
	go wsHub.Run()

	// Voice manager (in-memory state + WS; LiveKit handles media).
	voiceMgr := voice.NewManager(wsHub)

	// Do not auto-leave voice on transient WS disconnects.
	// LiveKit webhook + explicit /voice/leave remain the source of truth for voice presence.

	r := chi.NewRouter()
	r.Use(middleware.Logger)
	r.Use(middleware.Recoverer)

	r.Get("/health", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok"))
	})

	auth.RegisterRoutes(r, authSvc)
	ws.RegisterRoutes(r, wsHub, authSvc)

	r.Route("/servers", func(r chi.Router) {
		r.Use(authSvc.RequireAuth)
		r.Get("/", servers.List(st))
		r.Post("/", servers.Create(st))
		r.Delete("/{id}", servers.Delete(st, wsHub))
	})

	r.Route("/channels", func(r chi.Router) {
		r.Use(authSvc.RequireAuth)
		r.Get("/", channels.List(st))
		r.Post("/", channels.Create(st, wsHub))
		r.Patch("/{id}", channels.Rename(st, wsHub))
	})

	r.Route("/members", func(r chi.Router) {
		r.Use(authSvc.RequireAuth)
		r.Get("/", members.List(st))
		r.Post("/", members.Invite(st, wsHub))
		r.Patch("/nickname", members.SetNickname(st, wsHub))
	})

	r.Route("/messages", func(r chi.Router) {
		r.Use(authSvc.RequireAuth)
		r.Get("/", messages.List(st))
		r.Post("/", messages.Create(st, wsHub))
	})

	r.Route("/users", func(r chi.Router) {
		r.Get("/avatar", users.GetAvatar(st))
		r.With(authSvc.RequireAuth).Post("/me/avatar", users.SetAvatar(st, wsHub))
	})

	r.Route("/media", func(r chi.Router) {
		r.Use(authSvc.RequireAuth)
		r.Post("/", media.Upload(st))
		r.Get("/{id}", media.Download(st))
	})

	r.Post("/voice/webhook", voice.WebhookHandler(voiceMgr, st, cfg))
	r.Route("/voice", func(r chi.Router) {
		r.Use(authSvc.RequireAuth)
		voice.RegisterRoutes(r, voiceMgr, st, cfg)
	})

	return &Server{
		Router: r,
	}
}
