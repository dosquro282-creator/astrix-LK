package store

import (
	"context"
	"errors"
)

var (
	ErrInviteNotFound      = errors.New("invite not found")
	ErrUserBanned          = errors.New("user banned")
	ErrServerAccessDenied  = errors.New("server access denied")
	ErrCannotModerateOwner = errors.New("cannot moderate server owner")
)

type ServerBanRow struct {
	UserID      int64  `json:"user_id"`
	Username    string `json:"username"`
	DisplayName string `json:"display_name"`
}

func (s *Store) GetServer(ctx context.Context, serverID int64) (ServerRow, error) {
	var row ServerRow
	err := s.DB.QueryRow(ctx,
		`SELECT id, name, owner_id FROM servers WHERE id = $1`,
		serverID,
	).Scan(&row.ID, &row.Name, &row.OwnerID)
	return row, err
}

func (s *Store) RenameServer(ctx context.Context, serverID int64, name string) (ServerRow, error) {
	var row ServerRow
	err := s.DB.QueryRow(ctx,
		`UPDATE servers SET name = $2 WHERE id = $1 RETURNING id, name, owner_id`,
		serverID, name,
	).Scan(&row.ID, &row.Name, &row.OwnerID)
	return row, err
}

func (s *Store) IsServerMember(ctx context.Context, serverID, userID int64) (bool, error) {
	var count int
	err := s.DB.QueryRow(ctx,
		`SELECT COUNT(*) FROM server_members WHERE server_id = $1 AND user_id = $2`,
		serverID, userID,
	).Scan(&count)
	return count > 0, err
}

func (s *Store) RemoveMemberFromServerAndPresence(ctx context.Context, serverID, userID int64) error {
	tx, err := s.DB.Begin(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback(ctx)

	if _, err := tx.Exec(ctx,
		`DELETE FROM voice_presence vp
		 USING channels c
		 WHERE vp.channel_id = c.id AND c.server_id = $1 AND vp.user_id = $2`,
		serverID, userID,
	); err != nil {
		return err
	}

	if _, err := tx.Exec(ctx,
		`DELETE FROM server_members WHERE server_id = $1 AND user_id = $2`,
		serverID, userID,
	); err != nil {
		return err
	}

	return tx.Commit(ctx)
}

func (s *Store) IsUserBanned(ctx context.Context, serverID, userID int64) (bool, error) {
	var count int
	err := s.DB.QueryRow(ctx,
		`SELECT COUNT(*) FROM server_bans WHERE server_id = $1 AND user_id = $2`,
		serverID, userID,
	).Scan(&count)
	return count > 0, err
}

func (s *Store) BanUserFromServer(ctx context.Context, serverID, userID, bannedBy int64) error {
	tx, err := s.DB.Begin(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback(ctx)

	var ownerID int64
	if err := tx.QueryRow(ctx,
		`SELECT owner_id FROM servers WHERE id = $1`,
		serverID,
	).Scan(&ownerID); err != nil {
		return err
	}
	if ownerID == userID {
		return ErrCannotModerateOwner
	}

	var displayName string
	if err := tx.QueryRow(ctx,
		`SELECT COALESCE(m.nickname, u.username)
		 FROM users u
		 LEFT JOIN server_members m ON m.server_id = $1 AND m.user_id = u.id
		 WHERE u.id = $2`,
		serverID, userID,
	).Scan(&displayName); err != nil {
		return err
	}

	if _, err := tx.Exec(ctx,
		`INSERT INTO server_bans (server_id, user_id, banned_by, display_name)
		 VALUES ($1, $2, $3, $4)
		 ON CONFLICT (server_id, user_id)
		 DO UPDATE SET banned_by = EXCLUDED.banned_by, display_name = EXCLUDED.display_name, created_at = NOW()`,
		serverID, userID, bannedBy, displayName,
	); err != nil {
		return err
	}

	if _, err := tx.Exec(ctx,
		`DELETE FROM voice_presence vp
		 USING channels c
		 WHERE vp.channel_id = c.id AND c.server_id = $1 AND vp.user_id = $2`,
		serverID, userID,
	); err != nil {
		return err
	}

	if _, err := tx.Exec(ctx,
		`DELETE FROM server_members WHERE server_id = $1 AND user_id = $2`,
		serverID, userID,
	); err != nil {
		return err
	}

	return tx.Commit(ctx)
}

func (s *Store) UnbanUserFromServer(ctx context.Context, serverID, userID int64) error {
	_, err := s.DB.Exec(ctx,
		`DELETE FROM server_bans WHERE server_id = $1 AND user_id = $2`,
		serverID, userID,
	)
	return err
}

func (s *Store) ListServerBans(ctx context.Context, serverID int64) ([]ServerBanRow, error) {
	rows, err := s.DB.Query(ctx,
		`SELECT u.id, u.username, COALESCE(NULLIF(b.display_name, ''), u.username)
		 FROM server_bans b
		 JOIN users u ON u.id = b.user_id
		 WHERE b.server_id = $1
		 ORDER BY u.username`,
		serverID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var list []ServerBanRow
	for rows.Next() {
		var row ServerBanRow
		if err := rows.Scan(&row.UserID, &row.Username, &row.DisplayName); err != nil {
			return nil, err
		}
		list = append(list, row)
	}
	return list, rows.Err()
}
