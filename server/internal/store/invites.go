package store

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/base64"
	"errors"

	"github.com/jackc/pgx/v5"
)

type ServerInviteRow struct {
	Token       string
	ServerID    int64
	ServerName  string
	OwnerID     int64
	InviterID   int64
	ChannelID   *int64
	ChannelName *string
}

func (s *Store) CreateServerInvite(
	ctx context.Context,
	serverID int64,
	channelID *int64,
	inviterID int64,
) (ServerInviteRow, error) {
	token, err := newInviteToken()
	if err != nil {
		return ServerInviteRow{}, err
	}

	if channelID != nil {
		var count int
		if err := s.DB.QueryRow(ctx,
			`SELECT COUNT(*) FROM channels WHERE id = $1 AND server_id = $2`,
			*channelID, serverID,
		).Scan(&count); err != nil {
			return ServerInviteRow{}, err
		}
		if count == 0 {
			channelID = nil
		}
	}

	if _, err := s.DB.Exec(ctx,
		`INSERT INTO server_invites (token, server_id, channel_id, inviter_id)
		 VALUES ($1, $2, $3, $4)`,
		token, serverID, channelID, inviterID,
	); err != nil {
		return ServerInviteRow{}, err
	}

	return s.GetServerInvite(ctx, token)
}

func (s *Store) GetServerInvite(ctx context.Context, token string) (ServerInviteRow, error) {
	var row ServerInviteRow
	var channelID sql.NullInt64
	var channelName sql.NullString
	err := s.DB.QueryRow(ctx,
		`SELECT i.token, s.id, s.name, s.owner_id, i.inviter_id, c.id, c.name
		 FROM server_invites i
		 JOIN servers s ON s.id = i.server_id
		 LEFT JOIN channels c ON c.id = i.channel_id
		 WHERE i.token = $1`,
		token,
	).Scan(
		&row.Token,
		&row.ServerID,
		&row.ServerName,
		&row.OwnerID,
		&row.InviterID,
		&channelID,
		&channelName,
	)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) || errors.Is(err, pgx.ErrNoRows) {
			return ServerInviteRow{}, ErrInviteNotFound
		}
		return ServerInviteRow{}, err
	}
	if channelID.Valid {
		value := channelID.Int64
		row.ChannelID = &value
	}
	if channelName.Valid {
		value := channelName.String
		row.ChannelName = &value
	}
	return row, nil
}

func (s *Store) AcceptServerInvite(ctx context.Context, token string, userID int64) (ServerInviteRow, error) {
	invite, err := s.GetServerInvite(ctx, token)
	if err != nil {
		return ServerInviteRow{}, err
	}

	banned, err := s.IsUserBanned(ctx, invite.ServerID, userID)
	if err != nil {
		return ServerInviteRow{}, err
	}
	if banned {
		return ServerInviteRow{}, ErrUserBanned
	}

	if _, err := s.DB.Exec(ctx,
		`INSERT INTO server_members (server_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING`,
		invite.ServerID, userID,
	); err != nil {
		return ServerInviteRow{}, err
	}

	return invite, nil
}

func newInviteToken() (string, error) {
	raw := make([]byte, 18)
	if _, err := rand.Read(raw); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(raw), nil
}
