package config

import (
	"bytes"
	"io"
	"sort"
	"strings"
	"sync"
)

// Redacted is the text that replaces a secret wherever one would appear.
const Redacted = "[redacted]"

// Secrets is the set of values that must never leave the gateway: everything
// substituted from the environment plus the kernel token. It is safe for
// concurrent use and can redact strings, byte slices and whole log streams.
type Secrets struct {
	mu     sync.RWMutex
	values []string
}

// NewSecrets returns an empty set.
func NewSecrets() *Secrets { return &Secrets{} }

// Add registers a value. Empty values are ignored; duplicates are kept once.
// Longer values are redacted first so a secret that contains another is
// replaced whole.
func (s *Secrets) Add(v string) {
	if v == "" {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, existing := range s.values {
		if existing == v {
			return
		}
	}
	s.values = append(s.values, v)
	sort.Slice(s.values, func(i, j int) bool { return len(s.values[i]) > len(s.values[j]) })
}

// Count returns how many distinct secrets are registered.
func (s *Secrets) Count() int {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return len(s.values)
}

// Redact replaces every occurrence of every secret in the string.
func (s *Secrets) Redact(in string) string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	for _, v := range s.values {
		in = strings.ReplaceAll(in, v, Redacted)
	}
	return in
}

// RedactBytes is Redact for byte slices; it returns the input unchanged when
// no secret occurs in it.
func (s *Secrets) RedactBytes(in []byte) []byte {
	s.mu.RLock()
	defer s.mu.RUnlock()
	for _, v := range s.values {
		if bytes.Contains(in, []byte(v)) {
			in = bytes.ReplaceAll(in, []byte(v), []byte(Redacted))
		}
	}
	return in
}

// Contains reports whether any secret occurs in the text, for tests that
// prove nothing leaked.
func (s *Secrets) Contains(text string) bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	for _, v := range s.values {
		if strings.Contains(text, v) {
			return true
		}
	}
	return false
}

type redactingWriter struct {
	s *Secrets
	w io.Writer
}

// Writer wraps an io.Writer so every write is redacted before it reaches the
// underlying stream. The log handler writes one record per call, which is
// what makes this a complete guarantee for structured logs.
func (s *Secrets) Writer(w io.Writer) io.Writer { return &redactingWriter{s: s, w: w} }

func (r *redactingWriter) Write(p []byte) (int, error) {
	out := r.s.RedactBytes(p)
	if _, err := r.w.Write(out); err != nil {
		return 0, err
	}
	return len(p), nil
}
