package remit

import (
	"context"
	"crypto/ed25519"
	"log/slog"
	"sync"
	"time"
)

// Fetcher returns the kernel's current signing key.
type Fetcher func(ctx context.Context) (keyID string, key ed25519.PublicKey, err error)

// KeyStore resolves key ids to public keys. It either holds a pinned key
// (KERNOS_PUBLIC_KEY) or fetches from the kernel: once at start, then every
// two seconds in the background until the first key is known, then on the
// refresh timer (hourly by default). A token naming an unknown key id
// triggers a refetch, at most once per minute once a key is held (rotation)
// and at most once per second before that (kernel still starting), so
// neither case needs a restart. Nothing here blocks the listener.
type KeyStore struct {
	mu          sync.RWMutex
	keys        map[string]ed25519.PublicKey
	pinned      ed25519.PublicKey
	fetch       Fetcher
	refresh     time.Duration
	retry       time.Duration
	minGap      time.Duration
	lastAttempt time.Time
	failing     bool
	now         func() time.Time
	log         *slog.Logger
}

// NewKeyStore builds a store that fetches with fetch and refreshes every
// refresh (an hour when zero).
func NewKeyStore(fetch Fetcher, refresh time.Duration, log *slog.Logger) *KeyStore {
	if refresh <= 0 {
		refresh = time.Hour
	}
	if log == nil {
		log = slog.Default()
	}
	return &KeyStore{keys: map[string]ed25519.PublicKey{}, fetch: fetch, refresh: refresh, retry: 2 * time.Second, minGap: time.Minute, now: time.Now, log: log}
}

// NewPinnedKeyStore builds a store that trusts exactly one key for every key
// id and never talks to the kernel.
func NewPinnedKeyStore(pub ed25519.PublicKey, log *slog.Logger) *KeyStore {
	if log == nil {
		log = slog.Default()
	}
	return &KeyStore{keys: map[string]ed25519.PublicKey{}, pinned: pub, now: time.Now, log: log}
}

// Pinned reports whether the store uses a pinned key.
func (k *KeyStore) Pinned() bool { return k.pinned != nil }

// Start makes one fetch attempt (so a running kernel is known before the
// first call) and then keeps fetching in the background: every retry
// interval until a key is known, every refresh interval afterwards. It
// returns at once; a kernel that is still starting only delays the first
// successful verification, never the listener. With a pinned key it does
// nothing. The loop stops when ctx ends.
func (k *KeyStore) Start(ctx context.Context) {
	if k.pinned != nil || k.fetch == nil {
		return
	}
	if err := k.Refresh(ctx); err != nil {
		k.log.Warn("kernel public key not available yet, retrying in the background", "error", err.Error())
	}
	go func() {
		for {
			wait := k.refresh
			if !k.HasKeys() {
				wait = k.retry
			}
			select {
			case <-ctx.Done():
				return
			case <-time.After(wait):
			}
			if err := k.Refresh(ctx); err != nil {
				k.log.Debug("kernel key refresh failed", "error", err.Error())
			}
		}
	}()
}

// HasKeys reports whether at least one key is known.
func (k *KeyStore) HasKeys() bool {
	k.mu.RLock()
	defer k.mu.RUnlock()
	return len(k.keys) > 0
}

// Refresh fetches the key now. A failure is logged once per streak at warn
// level and at debug level while it persists.
func (k *KeyStore) Refresh(ctx context.Context) error {
	k.mu.Lock()
	k.lastAttempt = k.now()
	k.mu.Unlock()
	fctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	id, key, err := k.fetch(fctx)
	if err != nil {
		k.mu.Lock()
		first := !k.failing
		k.failing = true
		k.mu.Unlock()
		if first {
			k.log.Warn("could not fetch the kernel public key", "error", err.Error())
		}
		return err
	}
	k.mu.Lock()
	k.keys[id] = key
	k.failing = false
	k.mu.Unlock()
	k.log.Info("kernel public key loaded", "key_id", id)
	return nil
}

// Known lists the key ids the store holds.
func (k *KeyStore) Known() []string {
	k.mu.RLock()
	defer k.mu.RUnlock()
	out := make([]string, 0, len(k.keys))
	for id := range k.keys {
		out = append(out, id)
	}
	return out
}

// PublicKey implements KeyResolver.
func (k *KeyStore) PublicKey(ctx context.Context, keyID string) (ed25519.PublicKey, bool) {
	if k.pinned != nil {
		return k.pinned, true
	}
	k.mu.RLock()
	key, ok := k.keys[keyID]
	last := k.lastAttempt
	gap := k.minGap
	if len(k.keys) == 0 {
		gap = time.Second
	}
	k.mu.RUnlock()
	if ok {
		return key, true
	}
	if k.fetch != nil && k.now().Sub(last) >= gap {
		if err := k.Refresh(ctx); err != nil {
			k.log.Warn("key refetch for unknown key id failed", "key_id", keyID, "error", err.Error())
		}
		k.mu.RLock()
		key, ok = k.keys[keyID]
		k.mu.RUnlock()
	}
	return key, ok
}
