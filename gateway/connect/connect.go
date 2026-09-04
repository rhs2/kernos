// Package connect is the public SDK for Kernos gateway connectors.
//
// A connector is a Go value that exposes a set of tools, executes them
// against one company system and derives the data scope of every call from
// the call's arguments. The gateway binary hosts connectors, verifies the
// remit before any connector code runs, and probes each connector's contract
// on a timer. Third-party connectors register a factory with Register and are
// compiled into the gateway binary by importing their package.
//
// The identifiers in this file are the contract fixed by the gateway
// specification (06-GATEWAY-API); the helpers in the other files of the
// package exist so that a connector author does not have to re-implement
// scope strings, contract checks or the JSON Schema subset.
package connect

import (
	"context"
	"errors"
	"fmt"
	"sort"
	"sync"
)

// ToolSpec describes one tool a connector exposes. ID is the full tool
// identifier `<connector>.<operation>`; the gateway lists it on /v1/tools and
// matches it against the remit's tool patterns.
type ToolSpec struct {
	// ID is `<connector>.<operation>` in lowercase with underscores.
	ID string
	// Description is shown to operators and to the reasoning layer.
	Description string
	// Writes is true when the operation changes state in the upstream system.
	// The gateway refuses write tools for the observe autonomy level and
	// requires an idempotency key for them.
	Writes bool
	// InputSchema is the JSON Schema subset the call's arguments must satisfy
	// (see ValidateSchema). A nil schema accepts any object.
	InputSchema map[string]any
	// Contract names the response fields the tool promises and their JSON
	// types; the canary loop checks probes against it.
	Contract Contract
	// ScopeDerivation says how the connector derives the scope of a call:
	// "table", "host", "path", "declared" or "none". With "none" the remit must
	// carry the literal scope `<connector>:*`.
	ScopeDerivation string
}

// Contract is the shape a tool promises to return: required field names
// mapped to a JSON type name, one of "string", "number", "bool", "object" and
// "list". Extra fields in a response are always fine.
type Contract struct {
	Required map[string]string `json:"required"`
}

// Connector is the interface every connector implements. Call receives the
// operation name (the part of the tool id after the connector name and the
// dot) and returns the result, the scopes the call touched and an error.
// Errors wrapped with ErrDeterministic tell the worker not to retry.
type Connector interface {
	// Name is the connector's name, the prefix of every tool id it exposes.
	Name() string
	// Tools lists the tools the connector exposes.
	Tools() []ToolSpec
	// Call executes one operation with the given arguments.
	Call(ctx context.Context, tool string, args map[string]any) (result map[string]any, scopes []string, err error)
	// Probe runs the connector's harmless probe operation for the canary loop.
	Probe(ctx context.Context) (result map[string]any, err error)
}

// Factory builds a connector from its gateway.json object. The map is the
// whole connector entry including "name" and "type", with every ${VAR}
// already substituted.
type Factory = func(cfg map[string]any) (Connector, error)

// ErrDeterministic marks a failure that will happen again with the same
// input. Wrap connector errors with it (fmt.Errorf("%w: ...", ErrDeterministic)
// or Deterministic) so the gateway answers 422 and the kernel does not retry.
var ErrDeterministic = errors.New("deterministic failure")

// Deterministic builds an error wrapped with ErrDeterministic from a format
// string, which is the usual way a connector reports a rejected request.
func Deterministic(format string, args ...any) error {
	return fmt.Errorf("%w: %s", ErrDeterministic, fmt.Sprintf(format, args...))
}

// IsDeterministic reports whether err is or wraps ErrDeterministic.
func IsDeterministic(err error) bool {
	return errors.Is(err, ErrDeterministic)
}

// ScopeDeriver is implemented by connectors that can compute the scopes of a
// call from its arguments before the call runs. The gateway uses it for the
// sixth remit check; a connector that does not implement it is treated as
// ScopeDerivation "none" and requires the literal `<connector>:*` scope.
type ScopeDeriver interface {
	Scopes(tool string, args map[string]any) ([]string, error)
}

// ProbeSpec is the harmless operation the canary loop runs against a
// connector and the contract its response must satisfy.
type ProbeSpec struct {
	Tool     string
	Args     map[string]any
	Contract Contract
}

// ProbeDescriber is implemented by connectors that know their own probe when
// the configuration does not name one. The boolean is false when the
// connector has nothing harmless to probe.
type ProbeDescriber interface {
	ProbeSpec() (ProbeSpec, bool)
}

// ToolRefresher is implemented by connectors whose tool list comes from the
// upstream system (an MCP server, for instance). The canary loop calls it on
// every probe to notice new required input parameters, which count as
// contract drift.
type ToolRefresher interface {
	RefreshTools(ctx context.Context) ([]ToolSpec, error)
}

// CallInfo carries the identifiers of the current call through the context so
// connectors can log with run_id and step and so test tools can count calls
// per idempotency key. Connectors must never use it for authorisation.
type CallInfo struct {
	RunID          string
	Step           string
	LeaseID        string
	IdempotencyKey string
	RemitID        string
}

type callInfoKey struct{}

// WithCallInfo attaches the call identifiers to a context.
func WithCallInfo(ctx context.Context, info CallInfo) context.Context {
	return context.WithValue(ctx, callInfoKey{}, info)
}

// CallInfoFrom returns the call identifiers attached with WithCallInfo, or the
// zero value when the context carries none.
func CallInfoFrom(ctx context.Context) CallInfo {
	if v, ok := ctx.Value(callInfoKey{}).(CallInfo); ok {
		return v
	}
	return CallInfo{}
}

var (
	registryMu sync.RWMutex
	registry   = map[string]Factory{}
)

// Register makes a connector type available to the gateway under typeName,
// the value of "type" in gateway.json. Registering a name twice replaces the
// earlier factory, which lets a test substitute a fake.
func Register(factory func(cfg map[string]any) (Connector, error), typeName string) {
	registryMu.Lock()
	defer registryMu.Unlock()
	registry[typeName] = factory
}

// Lookup returns the factory registered for a connector type.
func Lookup(typeName string) (Factory, bool) {
	registryMu.RLock()
	defer registryMu.RUnlock()
	f, ok := registry[typeName]
	return f, ok
}

// Types lists the registered connector type names in sorted order, for
// diagnostics and for the gateway's startup log.
func Types() []string {
	registryMu.RLock()
	defer registryMu.RUnlock()
	out := make([]string, 0, len(registry))
	for k := range registry {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}
