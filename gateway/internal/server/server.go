// Package server is the gateway's HTTP surface: /v1/health, /v1/tools,
// /v1/tools/call, /v1/canaries, /v1/canaries/{connector}/probe and
// /release, and /v1/metrics, with the bodies and status codes of
// 06-GATEWAY-API. It wires the remit checks, the idempotency store, the
// circuit breakers and the canary manager around the connectors.
package server

import (
	"context"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
	"github.com/rhs2/kernos/gateway/internal/breaker"
	"github.com/rhs2/kernos/gateway/internal/canary"
	"github.com/rhs2/kernos/gateway/internal/config"
	"github.com/rhs2/kernos/gateway/internal/idem"
	"github.com/rhs2/kernos/gateway/internal/remit"
)

// Version is reported on /v1/health; it is the engine's semantic version.
const Version = "0.1.0"

// EventPoster appends tool.refused events to a run. The kernel client
// implements it; tests substitute a recorder.
type EventPoster interface {
	PostRefused(ctx context.Context, runID, remitToken string, payload map[string]any) error
}

// Built is a connector ready to serve, with the probe the canary loop runs.
type Built struct {
	Connector connect.Connector
	Probe     connect.ProbeSpec
	HasProbe  bool
}

// Options are the dependencies of a Server.
type Options struct {
	Config     *config.Config
	Logger     *slog.Logger
	Secrets    *config.Secrets
	Verifier   *remit.Verifier
	Kernel     EventPoster
	Idem       *idem.Store
	Canary     *canary.Manager
	Connectors []Built
	Now        func() time.Time
	// Breaker overrides the breaker options, for tests with a fake clock.
	Breaker breaker.Options
}

type connEntry struct {
	name    string
	conn    connect.Connector
	breaker *breaker.Breaker
	deriver connect.ScopeDeriver
}

type toolEntry struct {
	spec connect.ToolSpec
	op   string
	conn *connEntry
}

// Server serves the gateway API.
type Server struct {
	cfg         *config.Config
	log         *slog.Logger
	secrets     *config.Secrets
	verifier    *remit.Verifier
	kernel      EventPoster
	idem        *idem.Store
	canary      *canary.Manager
	now         func() time.Time
	conns       map[string]*connEntry
	connOrder   []string
	tools       map[string]*toolEntry
	toolOrder   []string
	metrics     *metrics
	locks       *keyedLocks
	callTimeout time.Duration
	mux         *http.ServeMux
	stop        context.CancelFunc
	wg          sync.WaitGroup
}

// New builds a server from its dependencies and indexes every tool.
func New(opts Options) (*Server, error) {
	if opts.Config == nil {
		opts.Config = config.Default()
	}
	if opts.Logger == nil {
		opts.Logger = slog.Default()
	}
	if opts.Secrets == nil {
		opts.Secrets = config.NewSecrets()
	}
	if opts.Verifier == nil {
		return nil, fmt.Errorf("server: a remit verifier is required")
	}
	if opts.Idem == nil {
		return nil, fmt.Errorf("server: an idempotency store is required")
	}
	if opts.Now == nil {
		opts.Now = time.Now
	}
	if opts.Canary == nil {
		opts.Canary = canary.New(canary.Options{Log: opts.Logger, Now: opts.Now})
	}
	s := &Server{
		cfg:         opts.Config,
		log:         opts.Logger,
		secrets:     opts.Secrets,
		verifier:    opts.Verifier,
		kernel:      opts.Kernel,
		idem:        opts.Idem,
		canary:      opts.Canary,
		now:         opts.Now,
		conns:       map[string]*connEntry{},
		tools:       map[string]*toolEntry{},
		metrics:     newMetrics(),
		locks:       newKeyedLocks(),
		callTimeout: time.Duration(opts.Config.CallTimeoutSeconds * float64(time.Second)),
	}
	if s.callTimeout <= 0 {
		s.callTimeout = config.DefaultCallTimeout * time.Second
	}
	for _, b := range opts.Connectors {
		name := b.Connector.Name()
		if !connect.ValidName(name) {
			return nil, fmt.Errorf("server: connector name %q is invalid", name)
		}
		if _, dup := s.conns[name]; dup {
			return nil, fmt.Errorf("server: duplicate connector %q", name)
		}
		bo := opts.Breaker
		if bo.Now == nil {
			bo.Now = opts.Now
		}
		entry := &connEntry{name: name, conn: b.Connector, breaker: breaker.New(bo)}
		if d, ok := b.Connector.(connect.ScopeDeriver); ok {
			entry.deriver = d
		}
		for _, spec := range b.Connector.Tools() {
			if !strings.HasPrefix(spec.ID, name+".") {
				return nil, fmt.Errorf("server: connector %q exposes tool %q outside its prefix", name, spec.ID)
			}
			op := strings.TrimPrefix(spec.ID, name+".")
			if !connect.ValidName(op) {
				return nil, fmt.Errorf("server: connector %q exposes tool %q with an invalid operation name", name, spec.ID)
			}
			if _, dup := s.tools[spec.ID]; dup {
				return nil, fmt.Errorf("server: duplicate tool %q", spec.ID)
			}
			if spec.ScopeDerivation == "" {
				spec.ScopeDerivation = connect.ScopeNone
			}
			s.tools[spec.ID] = &toolEntry{spec: spec, op: op, conn: entry}
			s.toolOrder = append(s.toolOrder, spec.ID)
		}
		s.conns[name] = entry
		s.connOrder = append(s.connOrder, name)
		s.canary.Add(canary.Target{Connector: b.Connector, Probe: b.Probe, HasProbe: b.HasProbe})
	}
	sort.Strings(s.toolOrder)
	s.routes()
	return s, nil
}

// Start runs the canary loop and the idempotency purge until Close.
func (s *Server) Start(ctx context.Context) {
	ctx, s.stop = context.WithCancel(ctx)
	s.canary.Start(ctx)
	s.wg.Add(1)
	go func() {
		defer s.wg.Done()
		s.purge(ctx)
		ticker := time.NewTicker(time.Hour)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				s.purge(ctx)
			}
		}
	}()
}

func (s *Server) purge(ctx context.Context) {
	n, err := s.idem.Purge(ctx)
	if err != nil {
		s.log.Warn("idempotency purge failed", "error", err.Error())
	} else if n > 0 {
		s.log.Info("idempotency entries purged", "count", n)
	}
}

// Close stops the loops, closes every connector that can be closed and the
// idempotency store.
func (s *Server) Close() error {
	if s.stop != nil {
		s.stop()
	}
	s.canary.Stop()
	s.wg.Wait()
	for _, name := range s.connOrder {
		if c, ok := s.conns[name].conn.(io.Closer); ok {
			if err := c.Close(); err != nil {
				s.log.Warn("connector close failed", "connector", name, "error", err.Error())
			}
		}
	}
	return s.idem.Close()
}

// Handler returns the HTTP handler with recovery and logging middleware.
func (s *Server) Handler() http.Handler {
	return s.recover(s.mux)
}

func (s *Server) routes() {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /v1/health", s.handleHealth)
	mux.HandleFunc("GET /v1/tools", s.handleTools)
	mux.HandleFunc("POST /v1/tools/call", s.handleCall)
	mux.HandleFunc("GET /v1/canaries", s.handleCanaries)
	mux.HandleFunc("POST /v1/canaries/{connector}/probe", s.handleProbe)
	mux.HandleFunc("POST /v1/canaries/{connector}/release", s.handleRelease)
	mux.HandleFunc("GET /v1/metrics", s.handleMetrics)
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		s.writeError(w, http.StatusNotFound, "not_found", "no such route", nil)
	})
	s.mux = mux
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	connectors := map[string]string{}
	for _, name := range s.connOrder {
		st, ok := s.canary.Get(name)
		if !ok {
			st.Status = canary.Healthy
		}
		connectors[name] = st.Status
	}
	s.writeJSON(w, http.StatusOK, map[string]any{"ok": true, "version": Version, "connectors": connectors})
}

func (s *Server) handleTools(w http.ResponseWriter, r *http.Request) {
	out := make([]map[string]any, 0, len(s.toolOrder))
	for _, id := range s.toolOrder {
		t := s.tools[id]
		schema := t.spec.InputSchema
		if schema == nil {
			schema = map[string]any{"type": "object"}
		}
		required := t.spec.Contract.Required
		if required == nil {
			required = map[string]string{}
		}
		out = append(out, map[string]any{
			"id":               id,
			"connector":        t.conn.name,
			"description":      t.spec.Description,
			"writes":           t.spec.Writes,
			"input_schema":     schema,
			"scope_derivation": t.spec.ScopeDerivation,
			"contract":         map[string]any{"required": required},
		})
	}
	s.writeJSON(w, http.StatusOK, out)
}

func (s *Server) handleCanaries(w http.ResponseWriter, r *http.Request) {
	s.writeJSON(w, http.StatusOK, s.canary.All())
}

func (s *Server) handleProbe(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("connector")
	if _, ok := s.conns[name]; !ok {
		s.writeError(w, http.StatusNotFound, "connector_not_found", "no connector named "+name, nil)
		return
	}
	st, err := s.canary.Probe(r.Context(), name)
	if err != nil {
		s.writeError(w, http.StatusNotFound, "connector_not_found", err.Error(), nil)
		return
	}
	s.writeJSON(w, http.StatusOK, st)
}

func (s *Server) handleRelease(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("connector")
	if _, ok := s.conns[name]; !ok {
		s.writeError(w, http.StatusNotFound, "connector_not_found", "no connector named "+name, nil)
		return
	}
	st, err := s.canary.Release(name)
	if err != nil {
		s.writeError(w, http.StatusNotFound, "connector_not_found", err.Error(), nil)
		return
	}
	s.log.Info("quarantine released by operator", "connector", name)
	s.writeJSON(w, http.StatusOK, st)
}

func (s *Server) handleMetrics(w http.ResponseWriter, r *http.Request) {
	var b strings.Builder
	s.metrics.render(&b)
	for _, name := range s.connOrder {
		st, _ := s.canary.Get(name)
		fmt.Fprintf(&b, "kernos_gateway_canary_status{connector=%q} %d\n", name, st.Value())
	}
	if len(s.connOrder) > 0 {
		b.WriteString("# HELP kernos_gateway_circuit_open 1 while the connector's circuit breaker rejects calls.\n# TYPE kernos_gateway_circuit_open gauge\n")
	}
	for _, name := range s.connOrder {
		v := 0
		if s.conns[name].breaker.IsOpen() {
			v = 1
		}
		fmt.Fprintf(&b, "kernos_gateway_circuit_open{connector=%q} %d\n", name, v)
	}
	w.Header().Set("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
	w.WriteHeader(http.StatusOK)
	w.Write(s.secrets.RedactBytes([]byte(b.String())))
}
