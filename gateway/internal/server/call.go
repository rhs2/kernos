package server

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
	"github.com/rhs2/kernos/gateway/internal/idem"
	"github.com/rhs2/kernos/gateway/internal/remit"
)

// maxCallBody bounds a /v1/tools/call request body.
const maxCallBody = 4 << 20

type callRequest struct {
	RemitToken     string         `json:"remit_token"`
	RunID          string         `json:"run_id"`
	Step           string         `json:"step"`
	LeaseID        string         `json:"lease_id"`
	Tool           string         `json:"tool"`
	Args           map[string]any `json:"args"`
	IdempotencyKey string         `json:"idempotency_key"`
	Scope          any            `json:"scope"`
}

// handleCall is POST /v1/tools/call: the seven remit checks in order, then
// argument validation, idempotency, quarantine, the circuit breaker and
// finally the connector.
func (s *Server) handleCall(w http.ResponseWriter, r *http.Request) {
	var req callRequest
	body, err := io.ReadAll(io.LimitReader(r.Body, maxCallBody+1))
	if err != nil {
		s.writeError(w, http.StatusBadRequest, "bad_request", "could not read the request body", nil)
		return
	}
	if len(body) > maxCallBody {
		s.writeError(w, http.StatusRequestEntityTooLarge, "bad_request", "request body exceeds 4 MiB", nil)
		return
	}
	if err := json.Unmarshal(body, &req); err != nil {
		s.writeError(w, http.StatusBadRequest, "bad_request", "request body is not the call object: "+err.Error(), nil)
		return
	}
	if req.Tool == "" {
		s.writeError(w, http.StatusBadRequest, "bad_request", "tool is required", nil)
		return
	}
	if req.Args == nil {
		req.Args = map[string]any{}
	}
	log := s.log.With("run_id", req.RunID, "step", req.Step, "tool", req.Tool, "lease_id", req.LeaseID)
	ctx := r.Context()
	now := s.now()

	// 1. Prefix, four parts, base64url.
	tok, ref := remit.Parse(req.RemitToken)
	if ref != nil {
		s.refuse(ctx, w, log, &req, nil, ref)
		return
	}
	// 2. Signature over the exact payload bytes, then decode.
	if ref := s.verifier.CheckSignature(ctx, tok); ref != nil {
		s.refuse(ctx, w, log, &req, tok, ref)
		return
	}
	log = log.With("remit_id", tok.RemitID())
	// 3. nbf <= now < exp.
	if ref := remit.CheckTime(tok, now); ref != nil {
		s.refuse(ctx, w, log, &req, tok, ref)
		return
	}
	// 4. A run-bound token only serves its run.
	if ref := remit.CheckRun(tok, req.RunID); ref != nil {
		s.refuse(ctx, w, log, &req, tok, ref)
		return
	}
	// 5. The tool matches a tools pattern.
	if ref := remit.CheckTool(tok, req.Tool); ref != nil {
		s.refuse(ctx, w, log, &req, tok, ref)
		return
	}
	entry, ok := s.tools[req.Tool]
	if !ok {
		s.metrics.call(req.Tool, outcomeNotFound)
		s.writeError(w, http.StatusNotFound, "tool_not_found", "no connector exposes "+req.Tool, nil)
		return
	}
	// 6. Every scope the connector derives from the arguments is granted.
	scopes, scopeErr := s.requiredScopes(entry, req.Args)
	if scopeErr == nil {
		if ref := remit.CheckScopes(tok, scopes); ref != nil {
			s.refuse(ctx, w, log, &req, tok, ref)
			return
		}
	}
	// 7. The writes flag is compatible with the autonomy level.
	if ref := remit.CheckAutonomy(tok, entry.spec.Writes); ref != nil {
		s.refuse(ctx, w, log, &req, tok, ref)
		return
	}

	// Arguments against the tool's input schema.
	if issues := connect.ValidateSchema(entry.spec.InputSchema, req.Args); len(issues) > 0 {
		s.metrics.call(req.Tool, outcomeArgsInvalid)
		log.Info("tool call rejected: args invalid", "issues", len(issues))
		s.writeError(w, http.StatusUnprocessableEntity, "args_invalid", "arguments do not satisfy the tool's input_schema", map[string]any{"details": map[string]any{"issues": issues}})
		return
	}
	if scopeErr != nil {
		s.metrics.call(req.Tool, outcomeArgsInvalid)
		log.Info("tool call rejected: scope could not be derived", "error", scopeErr.Error())
		s.writeError(w, http.StatusUnprocessableEntity, "args_invalid", "the connector could not derive a scope from the arguments", map[string]any{"details": map[string]any{"scope": s.secrets.Redact(scopeErr.Error())}})
		return
	}
	if entry.spec.Writes && req.IdempotencyKey == "" {
		s.metrics.call(req.Tool, outcomeArgsInvalid)
		log.Info("tool call rejected: write without idempotency key")
		s.writeError(w, http.StatusUnprocessableEntity, "idempotency_key_required", req.Tool+" writes, so the call needs an idempotency_key", nil)
		return
	}
	scopeText := strings.Join(scopes, ",")

	// Idempotency: same key and args replays, different args conflicts.
	argsHash := idem.HashArgs(req.Args)
	if req.IdempotencyKey != "" {
		unlock := s.locks.lock(req.Tool + "\x00" + req.IdempotencyKey)
		defer unlock()
		stored, err := s.idem.Lookup(ctx, req.Tool, req.IdempotencyKey)
		if err != nil {
			log.Error("idempotency lookup failed", "error", err.Error())
			s.writeError(w, http.StatusInternalServerError, "internal_error", "idempotency store unavailable", nil)
			return
		}
		if stored != nil {
			if stored.ArgsHash != argsHash {
				s.metrics.call(req.Tool, outcomeConflict)
				log.Warn("idempotency conflict", "idempotency_key", req.IdempotencyKey)
				s.writeError(w, http.StatusConflict, "idempotency_conflict", "idempotency_key was already used with different arguments", map[string]any{"idempotency_key": req.IdempotencyKey})
				return
			}
			s.metrics.call(req.Tool, outcomeReplayed)
			log.Info("tool call replayed", "idempotency_key", req.IdempotencyKey)
			s.writeJSON(w, http.StatusOK, map[string]any{"ok": true, "result": json.RawMessage(stored.Result), "scope": scopeText, "replayed": true, "latency_ms": 0})
			return
		}
	}

	// Quarantine and circuit breaker.
	if since, quarantined := s.canary.Quarantined(entry.conn.name); quarantined {
		s.metrics.call(req.Tool, outcomeQuarantined)
		log.Warn("tool call rejected: connector quarantined", "connector", entry.conn.name, "since", since)
		s.writeError(w, http.StatusServiceUnavailable, "connector_quarantined", "connector "+entry.conn.name+" is quarantined by its canary", map[string]any{"connector": entry.conn.name, "since": since, "deterministic": false})
		return
	}
	allowed, state := entry.conn.breaker.Allow()
	if !allowed {
		s.metrics.call(req.Tool, outcomeCircuitOpen)
		log.Warn("tool call rejected: circuit open", "connector", entry.conn.name, "state", string(state))
		s.writeError(w, http.StatusBadGateway, "upstream_error", "connector "+entry.conn.name+" is failing and its circuit is open", map[string]any{"circuit": "open", "connector": entry.conn.name, "deterministic": false})
		return
	}

	// The call. Writes run detached from the client connection so a worker
	// dying mid-call cannot leave a half-recorded write behind.
	callCtx := ctx
	if entry.spec.Writes {
		callCtx = context.WithoutCancel(ctx)
	}
	callCtx, cancel := context.WithTimeout(callCtx, s.callTimeout)
	defer cancel()
	callCtx = connect.WithCallInfo(callCtx, connect.CallInfo{RunID: req.RunID, Step: req.Step, LeaseID: req.LeaseID, IdempotencyKey: req.IdempotencyKey, RemitID: tok.RemitID()})
	start := time.Now()
	result, gotScopes, err := entry.conn.conn.Call(callCtx, entry.op, req.Args)
	latency := time.Since(start)
	s.metrics.observe(latency.Seconds())
	if err != nil {
		if connect.IsDeterministic(err) {
			s.metrics.call(req.Tool, outcomeDeterministic)
			log.Info("tool call failed deterministically", "error", s.secrets.Redact(err.Error()), "latency_ms", latency.Milliseconds())
			s.writeError(w, http.StatusUnprocessableEntity, "deterministic_failure", err.Error(), map[string]any{"deterministic": true, "connector": entry.conn.name})
			return
		}
		entry.conn.breaker.Failure()
		s.metrics.call(req.Tool, outcomeUpstream)
		extra := map[string]any{"deterministic": false, "connector": entry.conn.name}
		if errors.Is(err, context.DeadlineExceeded) {
			extra["timeout"] = true
		}
		log.Warn("tool call failed upstream", "error", s.secrets.Redact(err.Error()), "latency_ms", latency.Milliseconds(), "breaker", string(entry.conn.breaker.State()))
		s.writeError(w, http.StatusBadGateway, "upstream_error", err.Error(), extra)
		return
	}
	entry.conn.breaker.Success()
	if result == nil {
		result = map[string]any{}
	}
	if len(gotScopes) > 0 && !sameSet(gotScopes, scopes) && entry.spec.ScopeDerivation != connect.ScopeNone {
		log.Warn("connector reported scopes that differ from the derived ones", "derived", scopes, "reported", gotScopes)
	}
	encoded, err := json.Marshal(result)
	if err != nil {
		s.metrics.call(req.Tool, outcomeDeterministic)
		s.writeError(w, http.StatusUnprocessableEntity, "deterministic_failure", "connector result is not encodable: "+err.Error(), map[string]any{"deterministic": true})
		return
	}
	if req.IdempotencyKey != "" {
		if err := s.idem.Save(callCtx, req.Tool, req.IdempotencyKey, argsHash, encoded); err != nil {
			log.Error("idempotency save failed", "error", err.Error())
		}
	}
	s.metrics.call(req.Tool, outcomeOK)
	log.Info("tool call ok", "latency_ms", latency.Milliseconds(), "scope", scopeText, "idempotency_key", req.IdempotencyKey)
	s.writeJSON(w, http.StatusOK, map[string]any{"ok": true, "result": json.RawMessage(encoded), "scope": scopeText, "replayed": false, "latency_ms": latency.Milliseconds()})
}

// requiredScopes derives the scopes a call needs before the connector runs:
// the connector's derivation from the arguments, or the literal
// `<connector>:*` when the tool declares none or the connector cannot
// derive one.
func (s *Server) requiredScopes(entry *toolEntry, args map[string]any) ([]string, error) {
	literal := []string{connect.LiteralScope(entry.conn.name)}
	if entry.spec.ScopeDerivation == connect.ScopeNone || entry.conn.deriver == nil {
		return literal, nil
	}
	scopes, err := entry.conn.deriver.Scopes(entry.op, args)
	if err != nil {
		return nil, err
	}
	if len(scopes) == 0 {
		return literal, nil
	}
	return connect.UniqueSorted(scopes), nil
}

func sameSet(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	set := map[string]bool{}
	for _, x := range a {
		set[x] = true
	}
	for _, y := range b {
		if !set[y] {
			return false
		}
	}
	return true
}

// refuse answers 403 with the refusal, counts it, and appends tool.refused
// to the run through the kernel (best effort: a kernel that is down is
// logged, never fatal).
func (s *Server) refuse(ctx context.Context, w http.ResponseWriter, log interface {
	Warn(string, ...any)
}, req *callRequest, tok *remit.Token, ref *remit.Refusal) {
	s.metrics.refusal(ref.Reason)
	s.metrics.call(req.Tool, outcomeRefused)
	var remitID any
	if id := tok.RemitID(); id != "" {
		remitID = id
	}
	log.Warn("tool call refused", "reason", ref.Reason, "detail", ref.Detail)
	if req.RunID != "" && s.kernel != nil {
		pctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), 3*time.Second)
		defer cancel()
		payload := map[string]any{"step": req.Step, "tool": req.Tool, "reason": ref.Reason, "remit_id": remitID, "detail": ref.Detail}
		if err := s.kernel.PostRefused(pctx, req.RunID, req.RemitToken, payload); err != nil {
			log.Warn("could not append tool.refused to the run", "error", s.secrets.Redact(err.Error()))
		}
	}
	s.writeJSON(w, http.StatusForbidden, map[string]any{"ok": false, "refusal": map[string]any{"reason": ref.Reason, "detail": ref.Detail}})
}

// String renders a call request for debug logs without the token.
func (c callRequest) String() string {
	return fmt.Sprintf("run=%s step=%s tool=%s key=%s", c.RunID, c.Step, c.Tool, c.IdempotencyKey)
}
