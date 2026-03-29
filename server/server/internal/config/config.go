package config

import "os"

type Config struct {
	HTTPAddr    string
	DatabaseURL string
	RedisAddr   string
	JWTSecret   string
	// LiveKit (voice)
	LiveKitURL    string // URL clients use to connect (e.g. ws://localhost:7880)
	LiveKitAPIKey string
	LiveKitSecret string
}

func getenv(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func Load() Config {
	return Config{
		HTTPAddr:       getenv("HTTP_ADDR", "0.0.0.0:8080"),
		DatabaseURL:    getenv("DATABASE_URL", "postgres://astrix:astrix@localhost:5432/astrix?sslmode=disable"),
		RedisAddr:      getenv("REDIS_ADDR", "localhost:6379"),
		JWTSecret:      getenv("JWT_SECRET", "change_me_in_prod"),
		LiveKitURL:    getenv("LIVEKIT_URL", "ws://localhost:7880"),
		LiveKitAPIKey: getenv("LIVEKIT_API_KEY", "astrixkey"),
		LiveKitSecret: getenv("LIVEKIT_API_SECRET", "astrixsecret"),
	}
}
