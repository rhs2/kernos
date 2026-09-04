package breaker

import (
	"testing"
	"time"
)

type fakeClock struct{ t time.Time }

func (c *fakeClock) now() time.Time          { return c.t }
func (c *fakeClock) advance(d time.Duration) { c.t = c.t.Add(d) }
func midRand() float64                       { return 0.5 }
func newTest(clock *fakeClock, r float64) *Breaker {
	return New(Options{Now: clock.now, Rand: func() float64 { return r }})
}

func TestOpensAfterFiveFailures(t *testing.T) {
	clock := &fakeClock{t: time.Unix(1_757_000_000, 0)}
	b := newTest(clock, 0.5)
	for i := 0; i < 4; i++ {
		b.Failure()
		if ok, st := b.Allow(); !ok || st != Closed {
			t.Fatalf("failure %d must not open the circuit", i+1)
		}
	}
	b.Success()
	for i := 0; i < 4; i++ {
		b.Failure()
	}
	if b.State() != Closed {
		t.Fatal("success must reset the consecutive count")
	}
	b.Failure()
	if b.State() != Open || !b.IsOpen() {
		t.Fatal("fifth consecutive failure must open the circuit")
	}
	if ok, st := b.Allow(); ok || st != Open {
		t.Fatal("open circuit must reject")
	}
	clock.advance(9 * time.Second)
	if ok, _ := b.Allow(); ok {
		t.Fatal("still open before 10 s")
	}
	clock.advance(1100 * time.Millisecond)
	ok, st := b.Allow()
	if !ok || st != HalfOpen {
		t.Fatalf("after 10 s the breaker must half-open, got %v %s", ok, st)
	}
	if ok, st := b.Allow(); ok || st != HalfOpen {
		t.Fatal("only one trial call is allowed while half-open")
	}
	b.Success()
	if b.State() != Closed {
		t.Fatal("trial success closes the circuit")
	}
	if ok, _ := b.Allow(); !ok {
		t.Fatal("closed circuit allows")
	}
}

func TestExponentialGrowthAndCap(t *testing.T) {
	clock := &fakeClock{t: time.Unix(1_757_000_000, 0)}
	b := newTest(clock, 0.5)
	for i := 0; i < 5; i++ {
		b.Failure()
	}
	expected := 10 * time.Second
	for round := 0; round < 8; round++ {
		if got := b.openFor; got != expected {
			t.Fatalf("round %d: open period %v, want %v", round, got, expected)
		}
		clock.advance(expected - time.Millisecond)
		if ok, _ := b.Allow(); ok {
			t.Fatalf("round %d: allowed before the period elapsed", round)
		}
		clock.advance(time.Millisecond)
		if ok, st := b.Allow(); !ok || st != HalfOpen {
			t.Fatalf("round %d: expected half-open", round)
		}
		b.Failure()
		if b.State() != Open {
			t.Fatalf("round %d: trial failure must re-open", round)
		}
		expected *= 2
		if expected > 5*time.Minute {
			expected = 5 * time.Minute
		}
	}
	if b.Opens() != 9 {
		t.Fatalf("opens = %d", b.Opens())
	}
}

func TestJitter(t *testing.T) {
	clock := &fakeClock{t: time.Unix(1_757_000_000, 0)}
	low := New(Options{Now: clock.now, Rand: func() float64 { return 0 }})
	high := New(Options{Now: clock.now, Rand: func() float64 { return 1 }})
	for i := 0; i < 5; i++ {
		low.Failure()
		high.Failure()
	}
	if low.openFor != 8*time.Second || high.openFor != 12*time.Second {
		t.Fatalf("jitter must span 20%% both ways: %v %v", low.openFor, high.openFor)
	}
	if jittered(time.Second, 0.2, midRand()) != time.Second {
		t.Fatal("mid random gives the nominal period")
	}
}

func TestLateFailureWhileOpenIsIgnored(t *testing.T) {
	clock := &fakeClock{t: time.Unix(1_757_000_000, 0)}
	b := newTest(clock, 0.5)
	for i := 0; i < 5; i++ {
		b.Failure()
	}
	first := b.openedAt
	clock.advance(time.Second)
	b.Failure()
	if b.openedAt != first || b.openFor != 10*time.Second {
		t.Fatal("a failure while open must not extend the period")
	}
}
