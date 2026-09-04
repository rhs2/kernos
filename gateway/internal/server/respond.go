package server

import (
	"encoding/json"
	"net/http"
	"runtime/debug"
	"sync"
)

// writeJSON encodes a body, redacts every registered secret from the bytes
// and writes it. Redacting the encoded bytes is what makes "a credential
// never appears in a response" a property of the server rather than of each
// handler.
func (s *Server) writeJSON(w http.ResponseWriter, status int, body any) {
	data, err := json.Marshal(body)
	if err != nil {
		s.log.Error("response encoding failed", "error", err.Error())
		data = []byte(`{"ok":false,"error":{"code":"internal_error","message":"response encoding failed"}}`)
		status = http.StatusInternalServerError
	}
	data = s.secrets.RedactBytes(data)
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	w.Write(append(data, '\n'))
}

// writeError writes the error envelope of 00-OVERVIEW with ok:false.
func (s *Server) writeError(w http.ResponseWriter, status int, code, message string, extra map[string]any) {
	e := map[string]any{"code": code, "message": s.secrets.Redact(message)}
	for k, v := range extra {
		e[k] = v
	}
	s.writeJSON(w, status, map[string]any{"ok": false, "error": e})
}

// recover turns a panic in a handler into a 500 with a logged stack so no
// panic is reachable from HTTP.
func (s *Server) recover(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if rec := recover(); rec != nil {
				s.log.Error("handler panicked", "path", r.URL.Path, "panic", s.secrets.Redact(stringify(rec)), "stack", string(debug.Stack()))
				s.writeError(w, http.StatusInternalServerError, "internal_error", "the gateway hit an internal error", nil)
			}
		}()
		next.ServeHTTP(w, r)
	})
}

func stringify(v any) string {
	switch x := v.(type) {
	case error:
		return x.Error()
	case string:
		return x
	}
	b, _ := json.Marshal(v)
	return string(b)
}

// keyedLocks serialises calls that share an idempotency key so a retry
// arriving while the first attempt is still running waits for it and then
// replays instead of writing twice.
type keyedLocks struct {
	mu    sync.Mutex
	locks map[string]*keyedLock
}

type keyedLock struct {
	mu   sync.Mutex
	refs int
}

func newKeyedLocks() *keyedLocks { return &keyedLocks{locks: map[string]*keyedLock{}} }

func (k *keyedLocks) lock(key string) func() {
	k.mu.Lock()
	l, ok := k.locks[key]
	if !ok {
		l = &keyedLock{}
		k.locks[key] = l
	}
	l.refs++
	k.mu.Unlock()
	l.mu.Lock()
	return func() {
		l.mu.Unlock()
		k.mu.Lock()
		l.refs--
		if l.refs == 0 {
			delete(k.locks, key)
		}
		k.mu.Unlock()
	}
}
