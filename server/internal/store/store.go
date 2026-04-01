package store

import (
	"context"
	"encoding/json"
	"log"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
)

type Store struct {
	DB *pgxpool.Pool
}

func New(ctx context.Context, databaseURL string) (*Store, error) {
	cfg, err := pgxpool.ParseConfig(databaseURL)
	if err != nil {
		return nil, err
	}

	db, err := pgxpool.NewWithConfig(ctx, cfg)
	if err != nil {
		return nil, err
	}

	s := &Store{DB: db}

	if err := s.runMigrations(ctx); err != nil {
		db.Close()
		return nil, err
	}

	return s, nil
}

func (s *Store) Close() {
	s.DB.Close()
}

func (s *Store) runMigrations(ctx context.Context) error {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	queries := []string{
		`CREATE TABLE IF NOT EXISTS users (
			id SERIAL PRIMARY KEY,
			username TEXT UNIQUE NOT NULL,
			password_hash TEXT NOT NULL,
			public_e2ee_key BYTEA,
			avatar BYTEA,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		);`,
		`CREATE TABLE IF NOT EXISTS servers (
			id SERIAL PRIMARY KEY,
			name TEXT NOT NULL,
			owner_id INTEGER NOT NULL REFERENCES users(id),
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		);`,
		`CREATE TABLE IF NOT EXISTS server_members (
			server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
			user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
			nickname TEXT,
			roles TEXT[] NOT NULL DEFAULT '{}',
			joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			PRIMARY KEY (server_id, user_id)
		);`,
		`CREATE TABLE IF NOT EXISTS channels (
			id SERIAL PRIMARY KEY,
			server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
			name TEXT NOT NULL,
			type TEXT NOT NULL,
			position INTEGER NOT NULL DEFAULT 0
		);`,
		`CREATE TABLE IF NOT EXISTS messages (
			id SERIAL PRIMARY KEY,
			channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
			author_id INTEGER NOT NULL REFERENCES users(id),
			ciphertext BYTEA NOT NULL,
			nonce BYTEA NOT NULL,
			attachments_meta JSONB,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			edited_at TIMESTAMPTZ
		);`,
		`CREATE TABLE IF NOT EXISTS channel_keys (
			channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
			user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
			encrypted_key BYTEA NOT NULL,
			updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			PRIMARY KEY (channel_id, user_id)
		);`,
		`CREATE TABLE IF NOT EXISTS media (
			id SERIAL PRIMARY KEY,
			uploader_id INTEGER NOT NULL REFERENCES users(id),
			server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
			filename TEXT NOT NULL,
			mime_type TEXT NOT NULL,
			data BYTEA NOT NULL,
			size_bytes INTEGER NOT NULL DEFAULT 0,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		);`,
		// Idempotent alterations for existing installs
		`ALTER TABLE users ADD COLUMN IF NOT EXISTS avatar BYTEA;`,
		`ALTER TABLE server_members ADD COLUMN IF NOT EXISTS nickname TEXT;`,
		// Voice presence — ephemeral per-session state (cleared on server start)
		`CREATE TABLE IF NOT EXISTS voice_presence (
			channel_id  INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
			user_id     INTEGER NOT NULL REFERENCES users(id)    ON DELETE CASCADE,
			mic_muted   BOOLEAN NOT NULL DEFAULT false,
			deafened    BOOLEAN NOT NULL DEFAULT false,
			cam_enabled BOOLEAN NOT NULL DEFAULT false,
			streaming   BOOLEAN NOT NULL DEFAULT false,
			updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			PRIMARY KEY (channel_id, user_id)
		);`,
		`ALTER TABLE voice_presence ADD COLUMN IF NOT EXISTS deafened BOOLEAN NOT NULL DEFAULT false;`,
		// Read receipts: last message id read per (user, channel). Persists across reconnects.
		`CREATE TABLE IF NOT EXISTS channel_reads (
			user_id         INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
			channel_id      INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
			last_message_id BIGINT NOT NULL DEFAULT 0,
			updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			PRIMARY KEY (user_id, channel_id)
		);`,
		`CREATE TABLE IF NOT EXISTS server_bans (
			server_id    INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
			user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
			banned_by    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
			display_name TEXT NOT NULL DEFAULT '',
			created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			PRIMARY KEY (server_id, user_id)
		);`,
		`CREATE TABLE IF NOT EXISTS server_invites (
			token      TEXT PRIMARY KEY,
			server_id  INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
			channel_id INTEGER REFERENCES channels(id) ON DELETE SET NULL,
			inviter_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		);`,
		`ALTER TABLE server_bans ADD COLUMN IF NOT EXISTS display_name TEXT NOT NULL DEFAULT '';`,
	}

	for _, q := range queries {
		if _, err := s.DB.Exec(ctx, q); err != nil {
			return err
		}
	}

	log.Println("database migrations applied")
	return nil
}

// ==================== Types ====================

type ServerRow struct {
	ID      int64
	Name    string
	OwnerID int64
}

type ChannelRow struct {
	ID       int64
	ServerID int64
	Name     string
	Type     string
}

type MemberRow struct {
	UserID      int64
	Username    string
	DisplayName string
	IsOwner     bool
}

type AttachmentMeta struct {
	MediaID   int64  `json:"media_id"`
	Filename  string `json:"filename"`
	MimeType  string `json:"mime_type"`
	SizeBytes int64  `json:"size_bytes"`
}

type MessageRow struct {
	ID             int64
	ChannelID      int64
	AuthorID       int64
	AuthorUsername string
	Content        string
	CreatedAt      string
	Attachments    []AttachmentMeta
	SeenBy         []int64 // filled by ListMessagesForChannel from channel_reads
}

type MediaRow struct {
	ID         int64
	UploaderID int64
	ServerID   int64
	Filename   string
	MimeType   string
	Data       []byte
	SizeBytes  int64
}

// ==================== Servers ====================

func (s *Store) ListServersForUser(ctx context.Context, userID int64) ([]ServerRow, error) {
	rows, err := s.DB.Query(ctx,
		`SELECT s.id, s.name, s.owner_id FROM servers s
		 INNER JOIN server_members m ON m.server_id = s.id
		 WHERE m.user_id = $1 ORDER BY s.id`,
		userID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []ServerRow
	for rows.Next() {
		var r ServerRow
		if err := rows.Scan(&r.ID, &r.Name, &r.OwnerID); err != nil {
			return nil, err
		}
		list = append(list, r)
	}
	return list, rows.Err()
}

func (s *Store) CreateServer(ctx context.Context, ownerID int64, name string) (ServerRow, error) {
	var r ServerRow
	err := s.DB.QueryRow(ctx,
		`INSERT INTO servers (name, owner_id) VALUES ($1, $2) RETURNING id, name, owner_id`,
		name, ownerID,
	).Scan(&r.ID, &r.Name, &r.OwnerID)
	if err != nil {
		return r, err
	}
	_, err = s.DB.Exec(ctx,
		`INSERT INTO server_members (server_id, user_id) VALUES ($1, $2)`,
		r.ID, ownerID,
	)
	return r, err
}

func (s *Store) GetServerOwner(ctx context.Context, serverID int64) (int64, error) {
	var ownerID int64
	err := s.DB.QueryRow(ctx,
		`SELECT owner_id FROM servers WHERE id = $1`, serverID,
	).Scan(&ownerID)
	return ownerID, err
}

func (s *Store) CountServerMembers(ctx context.Context, serverID int64) (int, error) {
	var count int
	err := s.DB.QueryRow(ctx,
		`SELECT COUNT(*) FROM server_members WHERE server_id = $1`, serverID,
	).Scan(&count)
	return count, err
}

func (s *Store) DeleteServer(ctx context.Context, serverID int64) error {
	_, err := s.DB.Exec(ctx, `DELETE FROM servers WHERE id = $1`, serverID)
	return err
}

func (s *Store) LeaveServer(ctx context.Context, serverID, userID int64) error {
	_, err := s.DB.Exec(ctx,
		`DELETE FROM server_members WHERE server_id = $1 AND user_id = $2`,
		serverID, userID,
	)
	return err
}

// ==================== Channels ====================

func (s *Store) ListChannelsForServer(ctx context.Context, serverID int64) ([]ChannelRow, error) {
	rows, err := s.DB.Query(ctx,
		`SELECT id, server_id, name, type FROM channels WHERE server_id = $1 ORDER BY position, id`,
		serverID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []ChannelRow
	for rows.Next() {
		var r ChannelRow
		if err := rows.Scan(&r.ID, &r.ServerID, &r.Name, &r.Type); err != nil {
			return nil, err
		}
		list = append(list, r)
	}
	return list, rows.Err()
}

func (s *Store) CreateChannel(ctx context.Context, serverID int64, name, channelType string) (ChannelRow, error) {
	var r ChannelRow
	err := s.DB.QueryRow(ctx,
		`INSERT INTO channels (server_id, name, type) VALUES ($1, $2, $3) RETURNING id, server_id, name, type`,
		serverID, name, channelType,
	).Scan(&r.ID, &r.ServerID, &r.Name, &r.Type)
	return r, err
}

func (s *Store) RenameChannel(ctx context.Context, channelID int64, name string) (ChannelRow, error) {
	var r ChannelRow
	err := s.DB.QueryRow(ctx,
		`UPDATE channels SET name = $2 WHERE id = $1 RETURNING id, server_id, name, type`,
		channelID, name,
	).Scan(&r.ID, &r.ServerID, &r.Name, &r.Type)
	return r, err
}

func (s *Store) GetChannelByID(ctx context.Context, channelID int64) (ChannelRow, error) {
	var r ChannelRow
	err := s.DB.QueryRow(ctx,
		`SELECT id, server_id, name, type FROM channels WHERE id = $1`,
		channelID,
	).Scan(&r.ID, &r.ServerID, &r.Name, &r.Type)
	return r, err
}

// ==================== Members ====================

func (s *Store) ListMembersForServer(ctx context.Context, serverID int64) ([]MemberRow, error) {
	ownerID, err := s.GetServerOwner(ctx, serverID)
	if err != nil {
		return nil, err
	}
	rows, err := s.DB.Query(ctx,
		`SELECT u.id, u.username, COALESCE(m.nickname, u.username), m.nickname IS NOT NULL
		 FROM users u
		 INNER JOIN server_members m ON m.user_id = u.id
		 WHERE m.server_id = $1 ORDER BY u.username`,
		serverID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []MemberRow
	for rows.Next() {
		var r MemberRow
		var hasNick bool
		if err := rows.Scan(&r.UserID, &r.Username, &r.DisplayName, &hasNick); err != nil {
			return nil, err
		}
		r.IsOwner = r.UserID == ownerID
		list = append(list, r)
	}
	return list, rows.Err()
}

func (s *Store) AddMemberToServer(ctx context.Context, serverID, userID int64) error {
	_, err := s.DB.Exec(ctx,
		`INSERT INTO server_members (server_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING`,
		serverID, userID,
	)
	return err
}

func (s *Store) UserExistsByID(ctx context.Context, userID int64) (bool, error) {
	var count int
	err := s.DB.QueryRow(ctx, `SELECT COUNT(*) FROM users WHERE id = $1`, userID).Scan(&count)
	return count > 0, err
}

func (s *Store) SetServerNickname(ctx context.Context, serverID, userID int64, nickname string) error {
	_, err := s.DB.Exec(ctx,
		`UPDATE server_members SET nickname = $3 WHERE server_id = $1 AND user_id = $2`,
		serverID, userID, nickname,
	)
	return err
}

func (s *Store) GetDisplayName(ctx context.Context, serverID, userID int64) (string, error) {
	var name string
	err := s.DB.QueryRow(ctx,
		`SELECT COALESCE(m.nickname, u.username)
		 FROM users u JOIN server_members m ON m.user_id = u.id
		 WHERE m.server_id = $1 AND u.id = $2`,
		serverID, userID,
	).Scan(&name)
	return name, err
}

// ==================== Messages ====================

func parseAttachments(raw []byte) []AttachmentMeta {
	if len(raw) == 0 {
		return nil
	}
	var result []AttachmentMeta
	_ = json.Unmarshal(raw, &result)
	return result
}

func (s *Store) CreateMessage(ctx context.Context, channelID, authorID int64, content string, attachments []AttachmentMeta) (MessageRow, error) {
	var r MessageRow
	var rawContent []byte
	var rawAttach []byte

	var attachJSON []byte
	if len(attachments) > 0 {
		b, err := json.Marshal(attachments)
		if err != nil {
			return r, err
		}
		attachJSON = b
	}

	err := s.DB.QueryRow(ctx,
		`WITH ins AS (
			INSERT INTO messages (channel_id, author_id, ciphertext, nonce, attachments_meta)
			VALUES ($1, $2, $3, $4, $5)
			RETURNING id, channel_id, author_id, ciphertext, attachments_meta, created_at
		)
		SELECT i.id, i.channel_id, i.author_id, u.username, i.ciphertext, i.attachments_meta,
		       to_char(i.created_at, 'DD.MM.YYYY HH24:MI')
		FROM ins i JOIN users u ON u.id = i.author_id`,
		channelID, authorID, []byte(content), make([]byte, 12), attachJSON,
	).Scan(&r.ID, &r.ChannelID, &r.AuthorID, &r.AuthorUsername, &rawContent, &rawAttach, &r.CreatedAt)
	if err != nil {
		return r, err
	}
	r.Content = string(rawContent)
	r.Attachments = parseAttachments(rawAttach)
	return r, nil
}

func (s *Store) ListMessagesForChannel(ctx context.Context, channelID int64) ([]MessageRow, error) {
	rows, err := s.DB.Query(ctx,
		`SELECT m.id, m.channel_id, m.author_id, u.username, m.ciphertext, m.attachments_meta,
		        to_char(m.created_at, 'DD.MM.YYYY HH24:MI')
		 FROM messages m JOIN users u ON u.id = m.author_id
		 WHERE m.channel_id = $1
		 ORDER BY m.created_at ASC LIMIT 200`,
		channelID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []MessageRow
	for rows.Next() {
		var r MessageRow
		var rawContent []byte
		var rawAttach []byte
		if err := rows.Scan(&r.ID, &r.ChannelID, &r.AuthorID, &r.AuthorUsername, &rawContent, &rawAttach, &r.CreatedAt); err != nil {
			return nil, err
		}
		r.Content = string(rawContent)
		r.Attachments = parseAttachments(rawAttach)
		list = append(list, r)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}
	// Attach seen_by from persisted channel_reads (read receipt persistence).
	readMap, _ := s.GetChannelReads(ctx, channelID)
	for i := range list {
		list[i].SeenBy = []int64{}
		for uid, lastID := range readMap {
			if lastID >= list[i].ID && list[i].AuthorID != uid {
				list[i].SeenBy = append(list[i].SeenBy, uid)
			}
		}
	}
	return list, nil
}

// ==================== Channel reads (read receipt persistence) ====================

// SetChannelRead records that userID has read up to messageID in channelID.
func (s *Store) SetChannelRead(ctx context.Context, userID, channelID, messageID int64) error {
	_, err := s.DB.Exec(ctx,
		`INSERT INTO channel_reads (user_id, channel_id, last_message_id, updated_at)
		 VALUES ($1, $2, $3, NOW())
		 ON CONFLICT (user_id, channel_id) DO UPDATE
		 SET last_message_id = GREATEST(channel_reads.last_message_id, $3), updated_at = NOW()`,
		userID, channelID, messageID,
	)
	return err
}

// GetChannelReads returns map user_id -> last_message_id for the channel.
func (s *Store) GetChannelReads(ctx context.Context, channelID int64) (map[int64]int64, error) {
	rows, err := s.DB.Query(ctx,
		`SELECT user_id, last_message_id FROM channel_reads WHERE channel_id = $1`,
		channelID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := make(map[int64]int64)
	for rows.Next() {
		var uid, lastID int64
		if err := rows.Scan(&uid, &lastID); err != nil {
			return nil, err
		}
		out[uid] = lastID
	}
	return out, rows.Err()
}

// ==================== Users / Avatars ====================

func (s *Store) GetAvatar(ctx context.Context, userID int64) ([]byte, error) {
	var data []byte
	err := s.DB.QueryRow(ctx, `SELECT avatar FROM users WHERE id = $1`, userID).Scan(&data)
	return data, err
}

func (s *Store) SetAvatar(ctx context.Context, userID int64, data []byte) error {
	_, err := s.DB.Exec(ctx, `UPDATE users SET avatar = $2 WHERE id = $1`, userID, data)
	return err
}

// ==================== Media ====================

func (s *Store) SaveMedia(ctx context.Context, uploaderID, serverID int64, filename, mimeType string, data []byte) (MediaRow, error) {
	var r MediaRow
	err := s.DB.QueryRow(ctx,
		`INSERT INTO media (uploader_id, server_id, filename, mime_type, data, size_bytes)
		 VALUES ($1, $2, $3, $4, $5, $6)
		 RETURNING id, uploader_id, server_id, filename, mime_type, size_bytes`,
		uploaderID, serverID, filename, mimeType, data, len(data),
	).Scan(&r.ID, &r.UploaderID, &r.ServerID, &r.Filename, &r.MimeType, &r.SizeBytes)
	return r, err
}

// ==================== Voice Presence ====================

type VoicePresenceRow struct {
	ChannelID  int64
	UserID     int64
	MicMuted   bool
	Deafened   bool
	CamEnabled bool
	Streaming  bool
}

// VoiceClearAll removes all voice presence rows. Called at server startup
// to discard stale presence from a previous run.
func (s *Store) VoiceClearAll(ctx context.Context) error {
	_, err := s.DB.Exec(ctx, `DELETE FROM voice_presence`)
	return err
}

// VoiceJoin upserts a presence row when a user enters a voice channel.
func (s *Store) VoiceJoin(ctx context.Context, channelID, userID int64) error {
	_, err := s.DB.Exec(ctx,
		`INSERT INTO voice_presence (channel_id, user_id, updated_at)
		 VALUES ($1, $2, NOW())
		 ON CONFLICT (channel_id, user_id) DO UPDATE
		   SET mic_muted = false, deafened = false, cam_enabled = false, streaming = false,
		       updated_at = NOW()`,
		channelID, userID,
	)
	return err
}

// VoiceLeave removes a presence row when a user exits a voice channel.
func (s *Store) VoiceLeave(ctx context.Context, channelID, userID int64) error {
	_, err := s.DB.Exec(ctx,
		`DELETE FROM voice_presence WHERE channel_id = $1 AND user_id = $2`,
		channelID, userID,
	)
	return err
}

// VoiceUpdateState updates mic/deafened/cam/streaming flags for a participant.
func (s *Store) VoiceUpdateState(ctx context.Context, channelID, userID int64, micMuted, deafened, camEnabled, streaming bool) error {
	_, err := s.DB.Exec(ctx,
		`UPDATE voice_presence
		 SET mic_muted = $3, deafened = $4, cam_enabled = $5, streaming = $6, updated_at = NOW()
		 WHERE channel_id = $1 AND user_id = $2`,
		channelID, userID, micMuted, deafened, camEnabled, streaming,
	)
	return err
}

// VoiceListPresence returns current participants in a voice channel.
func (s *Store) VoiceListPresence(ctx context.Context, channelID int64) ([]VoicePresenceRow, error) {
	rows, err := s.DB.Query(ctx,
		`SELECT channel_id, user_id, mic_muted, deafened, cam_enabled, streaming
		 FROM voice_presence WHERE channel_id = $1`,
		channelID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var list []VoicePresenceRow
	for rows.Next() {
		var r VoicePresenceRow
		if err := rows.Scan(&r.ChannelID, &r.UserID, &r.MicMuted, &r.Deafened, &r.CamEnabled, &r.Streaming); err != nil {
			return nil, err
		}
		list = append(list, r)
	}
	return list, rows.Err()
}

func (s *Store) GetMedia(ctx context.Context, mediaID int64) (MediaRow, error) {
	var r MediaRow
	err := s.DB.QueryRow(ctx,
		`SELECT id, uploader_id, server_id, filename, mime_type, data, size_bytes FROM media WHERE id = $1`,
		mediaID,
	).Scan(&r.ID, &r.UploaderID, &r.ServerID, &r.Filename, &r.MimeType, &r.Data, &r.SizeBytes)
	return r, err
}
