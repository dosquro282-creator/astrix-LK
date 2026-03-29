package auth

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"net/http"
	"strings"
	"time"

	"astrix/server/internal/store"

	"github.com/go-chi/chi/v5"
	"github.com/golang-jwt/jwt/v5"
	"golang.org/x/crypto/bcrypt"
)

type contextKey string

const UserIDKey contextKey = "user_id"
const UserNameKey contextKey = "username"

func (s *Service) RequireAuth(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		auth := r.Header.Get("Authorization")
		if auth == "" || !strings.HasPrefix(auth, "Bearer ") {
			http.Error(w, "missing or invalid authorization", http.StatusUnauthorized)
			return
		}
		tokenStr := strings.TrimPrefix(auth, "Bearer ")
		token, err := s.ParseToken(tokenStr)
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
		ctx := context.WithValue(r.Context(), UserIDKey, int64(sub))
		if usr, ok := claims["usr"].(string); ok {
			ctx = context.WithValue(ctx, UserNameKey, usr)
		}
		next.ServeHTTP(w, r.WithContext(ctx))
	})
}

type Service struct {
	store     *store.Store
	jwtSecret []byte
}

func NewService(st *store.Store, jwtSecret []byte) *Service {
	return &Service{store: st, jwtSecret: jwtSecret}
}

type User struct {
	ID           int64
	Username     string
	PasswordHash string
	PublicE2EE   []byte
}

type RegisterRequest struct {
	Username     string `json:"username"`
	Password     string `json:"password"`
	PublicE2EE   []byte `json:"public_e2ee_key"`
}

type LoginRequest struct {
	Username string `json:"username"`
	Password string `json:"password"`
}

type TokenResponse struct {
	AccessToken string `json:"access_token"`
	UserID      int64  `json:"user_id"`
	Username    string `json:"username"`
}

func (s *Service) Register(ctx context.Context, req RegisterRequest) error {
	if req.Username == "" || req.Password == "" {
		return errors.New("username and password required")
	}

	hash, err := bcrypt.GenerateFromPassword([]byte(req.Password), bcrypt.DefaultCost)
	if err != nil {
		return err
	}

	_, err = s.store.DB.Exec(ctx,
		`INSERT INTO users (username, password_hash, public_e2ee_key) VALUES ($1, $2, $3)`,
		req.Username, string(hash), req.PublicE2EE,
	)
	return err
}

func (s *Service) Login(ctx context.Context, req LoginRequest) (TokenResponse, error) {
	var u User
	row := s.store.DB.QueryRow(ctx,
		`SELECT id, username, password_hash FROM users WHERE username = $1`,
		req.Username,
	)

	err := row.Scan(&u.ID, &u.Username, &u.PasswordHash)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return TokenResponse{}, errors.New("invalid credentials")
		}
		return TokenResponse{}, err
	}

	if err := bcrypt.CompareHashAndPassword([]byte(u.PasswordHash), []byte(req.Password)); err != nil {
		return TokenResponse{}, errors.New("invalid credentials")
	}

	token := jwt.NewWithClaims(jwt.SigningMethodHS256, jwt.MapClaims{
		"sub": u.ID,
		"usr": u.Username,
		"exp": time.Now().Add(24 * time.Hour).Unix(),
	})

	signed, err := token.SignedString(s.jwtSecret)
	if err != nil {
		return TokenResponse{}, err
	}

	return TokenResponse{AccessToken: signed, UserID: u.ID, Username: u.Username}, nil
}

func (s *Service) ParseToken(tokenStr string) (*jwt.Token, error) {
	return jwt.Parse(tokenStr, func(token *jwt.Token) (interface{}, error) {
		return s.jwtSecret, nil
	})
}

func RegisterRoutes(r chi.Router, svc *Service) {
	r.Route("/auth", func(r chi.Router) {
		r.Post("/register", func(w http.ResponseWriter, req *http.Request) {
			var body RegisterRequest
			if err := json.NewDecoder(req.Body).Decode(&body); err != nil {
				http.Error(w, "bad request", http.StatusBadRequest)
				return
			}
			if err := svc.Register(req.Context(), body); err != nil {
				http.Error(w, err.Error(), http.StatusBadRequest)
				return
			}
			w.WriteHeader(http.StatusCreated)
		})

		r.Post("/login", func(w http.ResponseWriter, req *http.Request) {
			var body LoginRequest
			if err := json.NewDecoder(req.Body).Decode(&body); err != nil {
				http.Error(w, "bad request", http.StatusBadRequest)
				return
			}
			tokens, err := svc.Login(req.Context(), body)
			if err != nil {
				http.Error(w, "unauthorized", http.StatusUnauthorized)
				return
			}
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(tokens)
		})
	})
}

