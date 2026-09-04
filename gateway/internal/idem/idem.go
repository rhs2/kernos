// Package idem is the gateway's idempotency store: a SQLite table keyed by
// (tool, idempotency_key) holding the hash of the arguments, the stored
// result and the creation time. It exists so that a worker retrying a write
// after a lost lease gets the earlier result back instead of a second write.
// Entries live thirty days.
package idem

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"time"

	_ "modernc.org/sqlite" // registers the "sqlite" driver
)

// TTL is how long an entry is honoured.
const TTL = 30 * 24 * time.Hour

// Entry is one stored call.
type Entry struct {
	Tool      string
	Key       string
	ArgsHash  string
	Result    json.RawMessage
	CreatedAt time.Time
}

// Store is the idempotency store. It is safe for concurrent use.
type Store struct {
	db  *sql.DB
	now func() time.Time
}

// Open creates or opens the store at path (":memory:" for tests) and makes
// sure the table exists.
func Open(path string) (*Store, error) {
	dsn := path
	if path != ":memory:" {
		dsn = "file:" + path + "?" + url.Values{"_pragma": []string{"busy_timeout(5000)", "journal_mode(WAL)"}}.Encode()
	}
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("open idempotency store: %w", err)
	}
	if path == ":memory:" {
		db.SetMaxOpenConns(1)
	}
	if _, err := db.Exec(`create table if not exists idempotency (
		tool text not null,
		key text not null,
		args_hash text not null,
		result blob not null,
		created_at text not null,
		primary key (tool, key)
	)`); err != nil {
		db.Close()
		return nil, fmt.Errorf("create idempotency table: %w", err)
	}
	if _, err := db.Exec(`create index if not exists idempotency_created on idempotency(created_at)`); err != nil {
		db.Close()
		return nil, fmt.Errorf("create idempotency index: %w", err)
	}
	return &Store{db: db, now: time.Now}, nil
}

// SetClock replaces the clock, for expiry tests.
func (s *Store) SetClock(now func() time.Time) { s.now = now }

// Close releases the database.
func (s *Store) Close() error { return s.db.Close() }

// HashArgs returns the sha256 hex of the arguments encoded as JSON with
// sorted keys, which encoding/json does for maps. Two calls with the same
// arguments in a different key order hash the same.
func HashArgs(args map[string]any) string {
	if args == nil {
		args = map[string]any{}
	}
	b, err := json.Marshal(args)
	if err != nil {
		b = []byte(fmt.Sprintf("%v", args))
	}
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:])
}

// Lookup returns the stored entry for (tool, key), or nil when there is none
// or the entry has expired.
func (s *Store) Lookup(ctx context.Context, tool, key string) (*Entry, error) {
	row := s.db.QueryRowContext(ctx, `select args_hash, result, created_at from idempotency where tool = ? and key = ?`, tool, key)
	var e Entry
	var created string
	if err := row.Scan(&e.ArgsHash, &e.Result, &created); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nil, nil
		}
		return nil, fmt.Errorf("idempotency lookup: %w", err)
	}
	t, err := time.Parse(time.RFC3339Nano, created)
	if err != nil {
		return nil, fmt.Errorf("idempotency lookup: bad created_at %q", created)
	}
	if s.now().Sub(t) >= TTL {
		return nil, nil
	}
	e.Tool, e.Key, e.CreatedAt = tool, key, t
	return &e, nil
}

// Save records the result of a completed call, replacing an expired entry
// with the same key.
func (s *Store) Save(ctx context.Context, tool, key, argsHash string, result json.RawMessage) error {
	_, err := s.db.ExecContext(ctx, `insert or replace into idempotency(tool, key, args_hash, result, created_at) values (?, ?, ?, ?, ?)`,
		tool, key, argsHash, []byte(result), s.now().UTC().Format(time.RFC3339Nano))
	if err != nil {
		return fmt.Errorf("idempotency save: %w", err)
	}
	return nil
}

// Purge deletes every entry older than the TTL and returns how many.
func (s *Store) Purge(ctx context.Context) (int64, error) {
	cutoff := s.now().Add(-TTL).UTC().Format(time.RFC3339Nano)
	res, err := s.db.ExecContext(ctx, `delete from idempotency where created_at < ?`, cutoff)
	if err != nil {
		return 0, fmt.Errorf("idempotency purge: %w", err)
	}
	n, _ := res.RowsAffected()
	return n, nil
}
