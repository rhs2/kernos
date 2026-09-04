// Package breaker implements the per-connector circuit breaker of the
// gateway specification: the circuit opens after five consecutive upstream
// errors, half-opens after ten seconds, and the open period doubles on every
// failed trial up to five minutes with twenty percent jitter. It exists so a
// failing upstream degrades one connector instead of tying up every worker
// in timeouts.
package breaker

import (
	"math/rand"
	"sync"
	"time"
)

// State is the breaker's current position.
type State string

// The three breaker states.
const (
	Closed   State = "closed"
	Open     State = "open"
	HalfOpen State = "half_open"
)

// Options tune a breaker. Zero values take the specification defaults.
type Options struct {
	// Threshold is the number of consecutive failures that opens the circuit.
	Threshold int
	// Base is the first open period; Max caps the exponential growth.
	Base, Max time.Duration
	// Jitter is the fraction of the period randomised both ways (0.2 = 20%).
	Jitter float64
	// Now and Rand are injectable for tests.
	Now  func() time.Time
	Rand func() float64
}

// Breaker is one circuit; it is safe for concurrent use.
type Breaker struct {
	mu        sync.Mutex
	opts      Options
	state     State
	failures  int
	openedAt  time.Time
	openFor   time.Duration
	backoff   time.Duration
	trialBusy bool
	opens     int64
}

// New builds a breaker with the given options.
func New(opts Options) *Breaker {
	if opts.Threshold <= 0 {
		opts.Threshold = 5
	}
	if opts.Base <= 0 {
		opts.Base = 10 * time.Second
	}
	if opts.Max <= 0 {
		opts.Max = 5 * time.Minute
	}
	if opts.Jitter < 0 {
		opts.Jitter = 0
	}
	if opts.Jitter == 0 {
		opts.Jitter = 0.2
	}
	if opts.Now == nil {
		opts.Now = time.Now
	}
	if opts.Rand == nil {
		opts.Rand = rand.Float64
	}
	return &Breaker{opts: opts, state: Closed, backoff: opts.Base}
}

// Allow reports whether a call may proceed and the state it was decided in.
// In the open state it returns false until the open period has elapsed, at
// which point the breaker half-opens and lets exactly one trial through.
func (b *Breaker) Allow() (bool, State) {
	b.mu.Lock()
	defer b.mu.Unlock()
	switch b.state {
	case Closed:
		return true, Closed
	case Open:
		if b.opts.Now().Sub(b.openedAt) < b.openFor {
			return false, Open
		}
		b.state = HalfOpen
		b.trialBusy = true
		return true, HalfOpen
	default:
		if b.trialBusy {
			return false, HalfOpen
		}
		b.trialBusy = true
		return true, HalfOpen
	}
}

// Success records a successful upstream call: it closes the circuit and
// resets the backoff.
func (b *Breaker) Success() {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.failures = 0
	b.trialBusy = false
	b.state = Closed
	b.backoff = b.opts.Base
}

// Failure records an upstream error. In the closed state it counts towards
// the threshold; in the half-open state it re-opens the circuit with a
// doubled period.
func (b *Breaker) Failure() {
	b.mu.Lock()
	defer b.mu.Unlock()
	switch b.state {
	case Closed:
		b.failures++
		if b.failures >= b.opts.Threshold {
			b.open(b.opts.Base)
		}
	case HalfOpen:
		next := b.backoff * 2
		if next > b.opts.Max {
			next = b.opts.Max
		}
		b.open(next)
	case Open:
		// A late failure from a call that started before the circuit opened
		// changes nothing.
	}
}

func (b *Breaker) open(period time.Duration) {
	b.state = Open
	b.trialBusy = false
	b.backoff = period
	b.openedAt = b.opts.Now()
	b.openFor = jittered(period, b.opts.Jitter, b.opts.Rand())
	b.opens++
}

func jittered(d time.Duration, jitter, r float64) time.Duration {
	factor := 1 + (r*2-1)*jitter
	return time.Duration(float64(d) * factor)
}

// State returns the current state without changing it (an elapsed open
// period still reads as open until Allow is called).
func (b *Breaker) State() State {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.state
}

// IsOpen reports whether calls are currently being rejected, for metrics.
func (b *Breaker) IsOpen() bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.state == Open {
		return b.opts.Now().Sub(b.openedAt) < b.openFor
	}
	return false
}

// Opens counts how many times the circuit has opened, for tests and logs.
func (b *Breaker) Opens() int64 {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.opens
}
