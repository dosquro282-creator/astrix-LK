package main

import (
	"context"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"astrix/server/internal/config"
	"astrix/server/internal/httpserver"
)

func main() {
	cfg := config.Load()

	srv := httpserver.NewServer(cfg)

	httpServer := &http.Server{
		Addr:    cfg.HTTPAddr,
		Handler: srv.Router,
	}

	go func() {
		log.Printf("Astrix server listening on %s\n", cfg.HTTPAddr)
		if err := httpServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Fatalf("server error: %v", err)
		}
	}()

	quit := make(chan os.Signal, 1)
	signal.Notify(quit, syscall.SIGINT, syscall.SIGTERM)
	<-quit

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	if err := httpServer.Shutdown(ctx); err != nil {
		log.Printf("server shutdown error: %v", err)
	}
}

